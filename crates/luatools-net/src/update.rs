//! Update checks against GitHub Releases and semver-ish version comparison.

use luatools_core::{Error, Result};
use reqwest::Client;
use serde::{Deserialize, Serialize};

/// One downloadable release asset.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReleaseAsset {
    /// File name as published.
    pub name: String,
    /// Browser download URL.
    pub download_url: String,
    /// SHA-256 hex digest when GitHub published one.
    pub digest: Option<String>,
    /// Size in bytes when reported.
    pub size: Option<u64>,
}

/// A newer application release.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Release {
    /// Version with any leading `v` removed, e.g. `1.4.0`.
    pub version: String,
    /// Release notes as plain text.
    pub notes: String,
    /// Published assets.
    pub assets: Vec<ReleaseAsset>,
}

/// Platform integration point for checking and applying updates.
///
/// [`GitHubReleases`] performs the network check asynchronously; a platform updater (for example
/// one driving an installer) implements this synchronous trait over its own local staging area.
pub trait UpdateBackend: Send + Sync {
    /// Returns the newer release when one exists.
    ///
    /// # Errors
    ///
    /// Implementations report discovery or transport failures.
    fn check(&self) -> Result<Option<Release>>;

    /// Applies an already staged release.
    ///
    /// # Errors
    ///
    /// Implementations report staging or installer failures.
    fn apply(&self) -> Result<()>;
}

/// Checks one or more GitHub repositories, in priority order, for a newer release.
///
/// The first reachable repository decides the answer. A repository that returns a release older
/// than or equal to the running version stops the search rather than falling through to a backup
/// that may lag further behind; backups exist for an unreachable primary, not for a stale one.
#[derive(Debug, Clone)]
pub struct GitHubReleases {
    client: Client,
    repos: Vec<(String, String)>,
    mirrors: crate::github::GithubMirrors,
    current: String,
}

impl GitHubReleases {
    /// Builds a checker for `repos` against `current_version`.
    ///
    /// A repository may be written as `owner/repo` or as `https://github.com/owner/repo`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidInput`] for an empty repository list or a malformed repository.
    pub fn new<I, S>(client: Client, repos: I, current_version: impl Into<String>) -> Result<Self>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut parsed = Vec::new();
        for repo in repos {
            parsed.push(parse_repo(repo.as_ref())?);
        }
        if parsed.is_empty() {
            return Err(Error::InvalidInput(
                "at least one GitHub release repository is required".to_owned(),
            ));
        }
        Ok(Self {
            client,
            repos: parsed,
            mirrors: crate::github::GithubMirrors::default(),
            current: current_version.into(),
        })
    }

    /// Replaces the mirror lists used for the API requests.
    #[must_use]
    pub fn with_mirrors(mut self, mirrors: crate::github::GithubMirrors) -> Self {
        self.mirrors = mirrors;
        self
    }

    /// The mirror lists used for the API requests.
    #[must_use]
    pub fn mirrors(&self) -> &crate::github::GithubMirrors {
        &self.mirrors
    }

    /// The version the running application reports.
    #[must_use]
    pub fn current_version(&self) -> &str {
        &self.current
    }

    /// The configured repositories as `owner/repo`, in priority order.
    #[must_use]
    pub fn repositories(&self) -> Vec<String> {
        self.repos
            .iter()
            .map(|(owner, repo)| format!("{owner}/{repo}"))
            .collect()
    }

    /// Returns a newer release, or `None` when the running version is current.
    ///
    /// # Errors
    ///
    /// Returns the last transport or parse failure when every repository is unreachable or
    /// malformed.
    pub async fn check(&self) -> Result<Option<Release>> {
        let mut last_failure: Option<Error> = None;

        for (owner, repo) in &self.repos {
            let url = format!("https://api.github.com/repos/{owner}/{repo}/releases/latest");
            let body = match crate::github::fetch_text_with(&self.client, &url, &self.mirrors).await
            {
                Ok(body) => body,
                Err(error) => {
                    last_failure = Some(error);
                    continue;
                }
            };
            let release = match parse_release(&body) {
                Ok(release) => release,
                Err(error) => {
                    last_failure = Some(error);
                    continue;
                }
            };
            return Ok(if is_newer(&self.current, &release.version) {
                Some(release)
            } else {
                None
            });
        }

        Err(last_failure
            .unwrap_or_else(|| Error::Other("no GitHub release repository answered".to_owned())))
    }
}

/// Parses a GitHub `releases/latest` body.
///
/// Assets without a name or download URL are dropped. A `sha256:` digest prefix is removed and
/// the remainder lowercased, matching the form produced by [`luatools_core::hash::sha256_file`].
///
/// # Errors
///
/// Returns [`Error::Parse`] when the body is missing fields or is not JSON, and rejects drafts
/// and prereleases so the stable channel cannot be offered a beta by accident.
pub fn parse_release(json: &str) -> Result<Release> {
    let raw: RawRelease = serde_json::from_str(json)
        .map_err(|error| Error::parse("github release", error.to_string()))?;

    if raw.tag_name.trim().is_empty() {
        return Err(Error::parse("github release", "empty tag name"));
    }
    if raw.draft {
        return Err(Error::parse("github release", "draft release"));
    }
    if raw.prerelease {
        return Err(Error::parse(
            "github release",
            "prerelease on the stable channel",
        ));
    }

    let version = normalize_version(&raw.tag_name);
    if version.is_empty() {
        return Err(Error::parse("github release", "empty version tag"));
    }

    let assets = raw
        .assets
        .into_iter()
        .filter_map(|asset| {
            let name = asset.name.filter(|name| !name.is_empty())?;
            let download_url = asset.browser_download_url.filter(|url| !url.is_empty())?;
            Some(ReleaseAsset {
                name,
                download_url,
                digest: asset.digest.and_then(normalize_digest),
                size: asset.size,
            })
        })
        .collect();

    Ok(Release {
        version,
        notes: raw.body.unwrap_or_default(),
        assets,
    })
}

/// Returns true when `candidate` is a later version than `current`.
///
/// Comparison is semver-ish: a leading `v` is ignored, build metadata after `+` is ignored,
/// numeric components compare numerically (so `1.10.0` is newer than `1.2.3`), and prereleases
/// sort before their matching release. Unparseable input is never considered newer.
#[must_use]
pub fn is_newer(current: &str, candidate: &str) -> bool {
    let (Some(current), Some(candidate)) = (Version::parse(current), Version::parse(candidate))
    else {
        return false;
    };
    candidate > current
}

fn normalize_version(tag: &str) -> String {
    let tag = tag.trim();
    tag.strip_prefix('v')
        .or_else(|| tag.strip_prefix('V'))
        .unwrap_or(tag)
        .to_owned()
}

fn normalize_digest(digest: String) -> Option<String> {
    let trimmed = digest.trim();
    if trimmed.is_empty() {
        return None;
    }
    let value = match trimmed.split_once(':') {
        Some((algorithm, value)) if algorithm.eq_ignore_ascii_case("sha256") => value,
        _ => trimmed,
    };
    let value = value.trim();
    if value.is_empty() {
        None
    } else {
        Some(value.to_ascii_lowercase())
    }
}

fn parse_repo(spec: &str) -> Result<(String, String)> {
    let spec = spec.trim().trim_end_matches('/');
    if spec.is_empty() {
        return Err(Error::InvalidInput("GitHub repository is empty".to_owned()));
    }

    let path = if spec.contains("://") {
        let parsed = url::Url::parse(spec)
            .map_err(|error| Error::InvalidInput(format!("GitHub repository: {error}")))?;
        if !matches!(parsed.host_str(), Some("github.com" | "www.github.com")) {
            return Err(Error::InvalidInput(
                "GitHub repository must be on github.com".to_owned(),
            ));
        }
        parsed.path().trim_matches('/').to_owned()
    } else {
        spec.to_owned()
    };

    let mut parts = path.split('/');
    let owner = parts.next().unwrap_or_default();
    let repo = parts.next().unwrap_or_default();
    if owner.is_empty() || repo.is_empty() || parts.next().is_some() {
        return Err(Error::InvalidInput(format!(
            "GitHub repository must be owner/repo: {spec}"
        )));
    }
    let repo = repo.strip_suffix(".git").unwrap_or(repo);
    if repo.is_empty() {
        return Err(Error::InvalidInput(format!(
            "GitHub repository must be owner/repo: {spec}"
        )));
    }
    Ok((owner.to_owned(), repo.to_owned()))
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Version {
    numbers: Vec<u64>,
    prerelease: Vec<PrereleaseId>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PrereleaseId {
    Numeric(u64),
    Text(String),
}

impl Ord for PrereleaseId {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        use std::cmp::Ordering;
        match (self, other) {
            (Self::Numeric(left), Self::Numeric(right)) => left.cmp(right),
            (Self::Numeric(_), Self::Text(_)) => Ordering::Less,
            (Self::Text(_), Self::Numeric(_)) => Ordering::Greater,
            (Self::Text(left), Self::Text(right)) => left.cmp(right),
        }
    }
}

impl PartialOrd for PrereleaseId {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Version {
    fn parse(text: &str) -> Option<Self> {
        let text = text.trim();
        let text = text
            .strip_prefix('v')
            .or_else(|| text.strip_prefix('V'))
            .unwrap_or(text);
        if text.is_empty() {
            return None;
        }

        let text = text.split_once('+').map_or(text, |(version, _)| version);
        let (core, prerelease) = text.split_once('-').unwrap_or((text, ""));

        let mut numbers = Vec::new();
        for part in core.split('.') {
            if part.is_empty() || !part.bytes().all(|byte| byte.is_ascii_digit()) {
                return None;
            }
            numbers.push(part.parse::<u64>().ok()?);
        }
        if numbers.is_empty() {
            return None;
        }

        let prerelease = if prerelease.is_empty() {
            Vec::new()
        } else {
            let mut identifiers = Vec::new();
            for identifier in prerelease.split('.') {
                if identifier.is_empty() {
                    return None;
                }
                if identifier.chars().all(|ch| ch.is_ascii_digit()) {
                    match identifier.parse::<u64>() {
                        Ok(number) => identifiers.push(PrereleaseId::Numeric(number)),
                        Err(_) => identifiers.push(PrereleaseId::Text(identifier.to_owned())),
                    }
                } else {
                    identifiers.push(PrereleaseId::Text(identifier.to_owned()));
                }
            }
            identifiers
        };

        Some(Self {
            numbers,
            prerelease,
        })
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        use std::cmp::Ordering;

        let length = self.numbers.len().max(other.numbers.len());
        for index in 0..length {
            let left = self.numbers.get(index).copied().unwrap_or(0);
            let right = other.numbers.get(index).copied().unwrap_or(0);
            match left.cmp(&right) {
                Ordering::Equal => {}
                ordering => return ordering,
            }
        }

        match (self.prerelease.is_empty(), other.prerelease.is_empty()) {
            (true, true) => Ordering::Equal,
            (true, false) => Ordering::Greater,
            (false, true) => Ordering::Less,
            (false, false) => self.prerelease.cmp(&other.prerelease),
        }
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

#[derive(Debug, Deserialize)]
struct RawRelease {
    tag_name: String,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
    #[serde(default)]
    assets: Vec<RawAsset>,
}

#[derive(Debug, Deserialize)]
struct RawAsset {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    browser_download_url: Option<String>,
    #[serde(default)]
    digest: Option<String>,
    #[serde(default)]
    size: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numeric_components_compare_as_numbers() {
        assert!(is_newer("1.2.3", "1.10.0"));
        assert!(!is_newer("1.10.0", "1.2.3"));
        assert!(is_newer("1.2.3", "1.2.10"));
        assert!(!is_newer("1.2.3", "1.2.3"));
    }

    #[test]
    fn leading_v_is_ignored() {
        assert!(is_newer("1.2.3", "v1.2.4"));
        assert!(is_newer("v1.2.3", "1.2.4"));
        assert!(!is_newer("v1.2.4", "v1.2.4"));
        assert!(is_newer("V1.0.0", "v2.0.0"));
    }

    #[test]
    fn missing_components_count_as_zero() {
        assert!(!is_newer("1.2", "1.2.0"));
        assert!(!is_newer("1.2.0", "1.2"));
        assert!(is_newer("1.2", "1.2.1"));
        assert!(is_newer("1.2.3", "1.2.3.1"));
    }

    #[test]
    fn prerelease_sorts_before_its_release() {
        assert!(is_newer("1.2.3-beta.1", "1.2.3"));
        assert!(!is_newer("1.2.3", "1.2.3-beta.1"));
        assert!(is_newer("1.2.3-beta.1", "1.2.3-beta.2"));
        assert!(is_newer("1.2.3-beta.2", "1.2.3-beta.10"));
        assert!(is_newer("1.2.3-alpha", "1.2.3-beta"));
        assert!(!is_newer("1.2.3-beta", "1.2.3-alpha"));
    }

    #[test]
    fn build_metadata_is_ignored() {
        assert!(!is_newer("1.2.3", "1.2.3+build.7"));
        assert!(is_newer("1.2.3", "1.2.4+build.7"));
    }

    #[test]
    fn unparseable_versions_are_never_newer() {
        assert!(!is_newer("1.2.3", "nightly"));
        assert!(!is_newer("", "1.0.0"));
        assert!(!is_newer("1.0.0", ""));
        assert!(!is_newer("release-1", "1.0.0"));
    }

    #[test]
    fn release_json_is_normalized() {
        let json = r#"{
            "tag_name":"v1.4.0",
            "body":"Notes",
            "draft":false,
            "prerelease":false,
            "assets":[
                {"name":"LuaTools.zip","browser_download_url":"https://github.com/o/r/a.zip",
                 "digest":"sha256:ABCDEF","size":123},
                {"name":"","browser_download_url":"https://github.com/o/r/b.zip"},
                {"name":"missing-url.zip","browser_download_url":""}
            ]
        }"#;
        let release = parse_release(json).expect("valid release");
        assert_eq!(release.version, "1.4.0");
        assert_eq!(release.notes, "Notes");
        assert_eq!(release.assets.len(), 1);
        assert_eq!(release.assets[0].digest.as_deref(), Some("abcdef"));
        assert_eq!(release.assets[0].size, Some(123));
    }

    #[test]
    fn drafts_and_prereleases_are_rejected() {
        assert!(parse_release(r#"{"tag_name":"v1","draft":true}"#).is_err());
        assert!(parse_release(r#"{"tag_name":"v1","prerelease":true}"#).is_err());
        assert!(parse_release("not json").is_err());
        assert!(parse_release(r#"{"tag_name":""}"#).is_err());
    }

    #[test]
    fn repository_specs_accept_urls_and_owner_repo() {
        assert_eq!(
            parse_repo("madoiscool/LuaTools").expect("owner/repo"),
            ("madoiscool".to_owned(), "LuaTools".to_owned())
        );
        assert_eq!(
            parse_repo("https://github.com/madoiscool/LuaTools/").expect("url"),
            ("madoiscool".to_owned(), "LuaTools".to_owned())
        );
        assert_eq!(
            parse_repo("https://github.com/o/r.git").expect("url"),
            ("o".to_owned(), "r".to_owned())
        );
        assert!(parse_repo("").is_err());
        assert!(parse_repo("owner").is_err());
        assert!(parse_repo("owner/repo/extra").is_err());
        assert!(parse_repo("https://gitlab.com/o/r").is_err());
    }

    #[test]
    fn github_releases_requires_a_repository() {
        let client = reqwest::Client::new();
        assert!(GitHubReleases::new(client, std::iter::empty::<&str>(), "1.0.0").is_err());
    }

    #[test]
    fn github_releases_exposes_its_configuration() {
        let client = reqwest::Client::new();
        let checker = GitHubReleases::new(
            client,
            [
                "madoiscool/LuaTools",
                "https://github.com/mendy-tools/LuaTools",
            ],
            "1.2.3",
        )
        .expect("valid configuration");
        assert_eq!(
            checker.repositories(),
            vec![
                "madoiscool/LuaTools".to_owned(),
                "mendy-tools/LuaTools".to_owned()
            ]
        );
        assert_eq!(checker.current_version(), "1.2.3");

        let mirrors =
            crate::github::GithubMirrors::new(vec!["https://api.mirror/".to_owned()], Vec::new());
        let checker = checker.with_mirrors(mirrors.clone());
        assert_eq!(checker.mirrors(), &mirrors);
    }
}
