//! GitHub requests with mirror fallback, plus streaming downloads.
//!
//! `github.com` and `api.github.com` are unreachable or throttled in some regions. Every request
//! is tried directly first and then through capability-matched mirrors: API metadata goes to the
//! API mirrors, downloads go to the public download mirrors. Mixing the classes wastes a round
//! trip because an API mirror rejects binary downloads and a download proxy rejects the REST API.

use std::path::{Path, PathBuf};

use futures_util::StreamExt;
use luatools_core::hash::HashDigest;
use luatools_core::{Error, Result};
use reqwest::Client;
use serde::de::DeserializeOwned;
use sha2::{Digest as _, Sha256};
use tokio::io::AsyncWriteExt as _;

/// Self-hosted mirrors able to serve `api.github.com` responses. Every entry must end with `/`.
pub const API_MIRRORS: &[&str] = &["https://lua.tools/api/gh/"];

/// Public mirrors able to serve GitHub release assets and raw files. Every entry must end with `/`.
pub const DOWNLOAD_MIRRORS: &[&str] = &[
    "https://ghproxy.net/",
    "https://ghfast.top/",
    "https://gh.ddlc.top/",
];

/// Mirror lists used when building candidate URLs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GithubMirrors {
    /// Mirrors that can serve `api.github.com` responses.
    pub api: Vec<String>,
    /// Mirrors that can serve release assets and raw files.
    pub download: Vec<String>,
}

impl Default for GithubMirrors {
    fn default() -> Self {
        Self {
            api: API_MIRRORS
                .iter()
                .map(|mirror| (*mirror).to_owned())
                .collect(),
            download: DOWNLOAD_MIRRORS
                .iter()
                .map(|mirror| (*mirror).to_owned())
                .collect(),
        }
    }
}

impl GithubMirrors {
    /// Builds an explicit mirror list.
    #[must_use]
    pub fn new(api: Vec<String>, download: Vec<String>) -> Self {
        Self { api, download }
    }
}

/// Returns true for hosts that the mirror lists apply to.
#[must_use]
pub fn is_github_url(url: &str) -> bool {
    let lower = url.to_ascii_lowercase();
    lower.starts_with("https://github.com/")
        || lower.starts_with("https://api.github.com/")
        || lower.starts_with("https://raw.githubusercontent.com/")
        || lower.starts_with("https://objects.githubusercontent.com/")
}

fn is_api_url(url: &str) -> bool {
    url.to_ascii_lowercase()
        .starts_with("https://api.github.com/")
}

fn build_candidates<'a>(
    url: &str,
    api: impl IntoIterator<Item = &'a str>,
    download: impl IntoIterator<Item = &'a str>,
) -> Vec<String> {
    let mut candidates = Vec::with_capacity(1 + DOWNLOAD_MIRRORS.len());
    candidates.push(url.to_owned());
    if !is_github_url(url) {
        return candidates;
    }
    let mirrors: Vec<&str> = if is_api_url(url) {
        api.into_iter().collect()
    } else {
        download.into_iter().collect()
    };
    candidates.extend(mirrors.into_iter().map(|mirror| format!("{mirror}{url}")));
    candidates
}

/// Candidate URLs to try in order: the direct URL first, then matching mirrors.
///
/// A non-GitHub URL yields just itself.
#[must_use]
pub fn candidates(url: &str) -> Vec<String> {
    build_candidates(
        url,
        API_MIRRORS.iter().copied(),
        DOWNLOAD_MIRRORS.iter().copied(),
    )
}

/// Like [`candidates`] but with caller-supplied mirror lists.
#[must_use]
pub fn candidates_with(url: &str, mirrors: &GithubMirrors) -> Vec<String> {
    build_candidates(
        url,
        mirrors.api.iter().map(String::as_str),
        mirrors.download.iter().map(String::as_str),
    )
}

/// Fetches `url` as text, trying direct then the matching mirrors.
///
/// # Errors
///
/// Returns [`Error::Network`] when every candidate fails; the message names the last failure.
pub async fn fetch_text(client: &Client, url: &str) -> Result<String> {
    fetch_text_candidates(client, url, candidates(url)).await
}

/// Like [`fetch_text`] but with caller-supplied mirror lists.
///
/// # Errors
///
/// Returns [`Error::Network`] when every candidate fails; the message names the last failure.
pub async fn fetch_text_with(
    client: &Client,
    url: &str,
    mirrors: &GithubMirrors,
) -> Result<String> {
    fetch_text_candidates(client, url, candidates_with(url, mirrors)).await
}

async fn fetch_text_candidates(
    client: &Client,
    url: &str,
    candidates: Vec<String>,
) -> Result<String> {
    let mut last_failure = String::from("no candidate attempted");

    for candidate in candidates {
        let response = client
            .get(&candidate)
            .header(reqwest::header::USER_AGENT, crate::http::USER_AGENT)
            .header(reqwest::header::ACCEPT, "application/vnd.github+json")
            .send()
            .await;

        match response {
            Ok(response) => {
                if response.status().is_success() {
                    return response.text().await.map_err(|error| Error::Network {
                        context: format!("reading {candidate}"),
                        message: error.to_string(),
                    });
                }
                last_failure = format!("{candidate}: http {}", response.status().as_u16());
            }
            Err(error) => last_failure = format!("{candidate}: {error}"),
        }
    }

    Err(Error::Network {
        context: format!("GET {url}"),
        message: last_failure,
    })
}

/// Fetches GitHub-flavoured JSON, trying direct then the matching mirrors.
///
/// # Errors
///
/// Propagates [`fetch_text`] failures and returns [`Error::Parse`] for invalid JSON.
pub async fn fetch_json<T: DeserializeOwned>(client: &Client, url: &str) -> Result<T> {
    let text = fetch_text(client, url).await?;
    serde_json::from_str(&text).map_err(|error| Error::parse("json", format!("{url}: {error}")))
}

/// Downloads `url` to `dest`, trying direct then the matching mirrors.
///
/// The body streams into a temporary file beside `dest`; the file is renamed into place only
/// after the transfer completes. `progress` receives bytes written and the response's
/// `Content-Length` when it is known. The returned digest is the SHA-256 of the bytes written.
///
/// # Errors
///
/// Returns [`Error::Http`] for an unsuccessful response, [`Error::Io`] when the destination
/// cannot be written, and [`Error::Network`] when every candidate fails.
pub async fn download<F>(client: &Client, url: &str, dest: &Path, progress: F) -> Result<HashDigest>
where
    F: FnMut(u64, Option<u64>),
{
    if let Some(parent) = dest
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|error| Error::io(format!("creating {}", parent.display()), error))?;
    }

    let mut progress = progress;
    let mut last_failure = String::from("no candidate attempted");

    for candidate in candidates(url) {
        match download_one(client, &candidate, dest, &mut progress).await {
            Ok(digest) => return Ok(digest),
            Err(error) => last_failure = error.to_string(),
        }
    }

    Err(Error::Network {
        context: format!("downloading {url} from GitHub or a mirror"),
        message: last_failure,
    })
}

async fn download_one<F>(
    client: &Client,
    url: &str,
    dest: &Path,
    progress: &mut F,
) -> Result<HashDigest>
where
    F: FnMut(u64, Option<u64>),
{
    let response = client
        .get(url)
        .header(reqwest::header::USER_AGENT, crate::http::USER_AGENT)
        .send()
        .await
        .map_err(|error| Error::Network {
            context: format!("GET {url}"),
            message: error.to_string(),
        })?;

    if !response.status().is_success() {
        return Err(Error::Http {
            status: response.status().as_u16(),
            url: url.to_owned(),
        });
    }

    let total = response.content_length();
    let temp = temp_path(dest);
    match tokio::fs::remove_file(&temp).await {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(Error::io(
                format!("clearing stale {}", temp.display()),
                error,
            ));
        }
    }

    let mut file = tokio::fs::File::create(&temp)
        .await
        .map_err(|error| Error::io(format!("creating {}", temp.display()), error))?;
    let mut hasher = Sha256::new();
    let mut written = 0_u64;
    let mut stream = response.bytes_stream();

    while let Some(next) = stream.next().await {
        let chunk = match next {
            Ok(chunk) => chunk,
            Err(error) => {
                drop(file);
                let _ = tokio::fs::remove_file(&temp).await;
                return Err(Error::Network {
                    context: format!("reading {url}"),
                    message: error.to_string(),
                });
            }
        };

        if let Err(error) = file.write_all(&chunk).await {
            drop(file);
            let _ = tokio::fs::remove_file(&temp).await;
            return Err(Error::io(format!("writing {}", temp.display()), error));
        }
        hasher.update(&chunk);
        written = written.saturating_add(chunk.len() as u64);
        progress(written, total);
    }

    if let Err(error) = file.flush().await {
        drop(file);
        let _ = tokio::fs::remove_file(&temp).await;
        return Err(Error::io(format!("flushing {}", temp.display()), error));
    }
    drop(file);

    if let Err(error) = replace_file(&temp, dest).await {
        let _ = tokio::fs::remove_file(&temp).await;
        return Err(error);
    }

    Ok(HashDigest::from_hex(hex::encode(hasher.finalize())))
}

async fn replace_file(temp: &Path, dest: &Path) -> Result<()> {
    if tokio::fs::rename(temp, dest).await.is_ok() {
        return Ok(());
    }
    match tokio::fs::remove_file(dest).await {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(Error::io(format!("replacing {}", dest.display()), error));
        }
    }
    tokio::fs::rename(temp, dest)
        .await
        .map_err(|error| Error::io(format!("replacing {}", dest.display()), error))
}

fn temp_path(dest: &Path) -> PathBuf {
    let name = dest
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("download");
    dest.with_file_name(format!(".{name}.{}.part", std::process::id()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn direct_url_comes_first() {
        let urls =
            candidates("https://github.com/madoiscool/LuaTools/releases/download/v1/plugin.zip");
        assert_eq!(
            urls.first().map(String::as_str),
            Some("https://github.com/madoiscool/LuaTools/releases/download/v1/plugin.zip")
        );
    }

    #[test]
    fn download_mirrors_follow_in_order() {
        let urls = candidates("https://github.com/o/r/releases/download/v1/a.zip");
        assert_eq!(
            urls,
            vec![
                "https://github.com/o/r/releases/download/v1/a.zip".to_owned(),
                format!(
                    "https://ghproxy.net/{}",
                    "https://github.com/o/r/releases/download/v1/a.zip"
                ),
                format!(
                    "https://ghfast.top/{}",
                    "https://github.com/o/r/releases/download/v1/a.zip"
                ),
                format!(
                    "https://gh.ddlc.top/{}",
                    "https://github.com/o/r/releases/download/v1/a.zip"
                ),
            ]
        );
    }

    #[test]
    fn api_urls_only_use_api_mirrors() {
        let urls = candidates("https://api.github.com/repos/o/r/releases/latest");
        assert_eq!(urls.len(), 2);
        assert_eq!(urls[0], "https://api.github.com/repos/o/r/releases/latest");
        assert_eq!(
            urls[1],
            "https://lua.tools/api/gh/https://api.github.com/repos/o/r/releases/latest"
        );
    }

    #[test]
    fn raw_and_objects_urls_use_download_mirrors() {
        for url in [
            "https://raw.githubusercontent.com/o/r/main/file.json",
            "https://objects.githubusercontent.com/github-production-release-asset/1",
        ] {
            let urls = candidates(url);
            assert_eq!(urls.len(), 4, "{url}");
            assert!(urls[1].starts_with("https://ghproxy.net/"));
        }
    }

    #[test]
    fn non_github_urls_are_not_proxied() {
        let urls = candidates("https://lua.tools/api/check");
        assert_eq!(urls, vec!["https://lua.tools/api/check".to_owned()]);
    }

    #[test]
    fn custom_mirrors_are_honoured() {
        let mirrors = GithubMirrors::new(
            vec!["https://api.mirror/".to_owned()],
            vec!["https://dl.mirror/".to_owned()],
        );
        assert_eq!(
            candidates_with("https://github.com/o/r", &mirrors),
            vec![
                "https://github.com/o/r".to_owned(),
                "https://dl.mirror/https://github.com/o/r".to_owned()
            ]
        );
        assert_eq!(
            candidates_with("https://api.github.com/repos/o/r", &mirrors),
            vec![
                "https://api.github.com/repos/o/r".to_owned(),
                "https://api.mirror/https://api.github.com/repos/o/r".to_owned()
            ]
        );
    }

    #[test]
    fn github_detection_is_case_insensitive() {
        assert!(is_github_url("HTTPS://GITHUB.COM/o/r"));
        assert!(!is_github_url("https://example.com/github.com"));
    }

    #[test]
    fn temp_file_sits_beside_the_destination() {
        let temp = temp_path(Path::new("/tmp/assets/plugin.zip"));
        assert_eq!(temp.parent(), Some(Path::new("/tmp/assets")));
        assert!(
            temp.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with(".plugin.zip.") && name.ends_with(".part"))
        );
    }
}
