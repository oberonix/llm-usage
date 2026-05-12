//! GitHub-release update check.
//!
//! The tray invokes [`check`] once at startup and every 24 hours after.
//! Returns `Some(UpdateInfo)` only when the latest non-draft,
//! non-prerelease tag on `github.com/oberonix/llm-usage` parses as a
//! semver version greater than the running binary's
//! `CARGO_PKG_VERSION`. Any network or parse failure quietly returns
//! `None` — the tray keeps running, we just don't surface a banner.
//!
//! This is the *only* outbound HTTP call the tray makes that isn't to
//! one of the user's data providers. It's gated behind
//! `Config.check_for_updates` (default `true`, easy to flip off).

use anyhow::{anyhow, Result};
use semver::Version;
use serde::Deserialize;
use std::time::Duration;

const REPO: &str = "oberonix/llm-usage";
const GITHUB_API: &str = "https://api.github.com";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateInfo {
    /// The new version without the leading `v` (e.g. `"0.2.0"`).
    pub version: String,
    /// `html_url` of the GitHub release — what we open when the user
    /// clicks the menu line.
    pub url: String,
}

#[derive(Deserialize)]
struct GitHubRelease {
    tag_name: String,
    html_url: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
}

/// Ask the GitHub API for the latest release of this repo and compare
/// it against `current_version` (typically `env!("CARGO_PKG_VERSION")`).
/// Returns `Ok(None)` when:
///   - the latest release is a draft or pre-release,
///   - the tag doesn't parse as semver,
///   - the running binary's version doesn't parse as semver, or
///   - the latest is not strictly newer than `current_version`.
pub async fn check(current_version: &str) -> Result<Option<UpdateInfo>> {
    check_with(GITHUB_API, current_version).await
}

/// Same as [`check`] but with the base URL injectable for tests.
/// `base` should be a scheme + host + optional port — e.g.
/// `"https://api.github.com"` in production or
/// `mock_server.uri()` from wiremock in tests. The repo path and
/// `/releases/latest` are appended internally.
pub async fn check_with(base: &str, current_version: &str) -> Result<Option<UpdateInfo>> {
    let url = format!(
        "{}/repos/{}/releases/latest",
        base.trim_end_matches('/'),
        REPO
    );
    let client = reqwest::Client::builder()
        .user_agent(concat!("llm-usage/", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(10))
        .build()?;
    let resp = client
        .get(&url)
        .header("accept", "application/vnd.github+json")
        .send()
        .await?;
    if !resp.status().is_success() {
        return Err(anyhow!("github releases API returned {}", resp.status()));
    }
    let release: GitHubRelease = resp.json().await?;
    if release.draft || release.prerelease {
        return Ok(None);
    }
    let tag = release.tag_name.trim_start_matches('v');
    let latest = match Version::parse(tag) {
        Ok(v) => v,
        Err(_) => return Ok(None),
    };
    let current = match Version::parse(current_version) {
        Ok(v) => v,
        Err(_) => return Ok(None),
    };
    if latest > current {
        Ok(Some(UpdateInfo {
            version: tag.to_string(),
            url: release.html_url,
        }))
    } else {
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[test]
    fn equal_versions_yield_no_update() {
        let cur = Version::parse("0.1.0").unwrap();
        let new = Version::parse("0.1.0").unwrap();
        assert!(new <= cur);
    }

    #[test]
    fn patch_bump_is_an_update() {
        let cur = Version::parse("0.1.0").unwrap();
        let new = Version::parse("0.1.1").unwrap();
        assert!(new > cur);
    }

    /// Stand up a wiremock server that replies to the latest-release
    /// path with the given JSON body and status. Returns the server
    /// so the caller can pass its URI into `check_with`.
    async fn mock_latest_release(status: u16, body: serde_json::Value) -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(format!("/repos/{}/releases/latest", REPO)))
            .and(header("accept", "application/vnd.github+json"))
            .respond_with(ResponseTemplate::new(status).set_body_json(body))
            .expect(1)
            .mount(&server)
            .await;
        server
    }

    #[tokio::test]
    async fn newer_tag_returns_update_info() {
        let server = mock_latest_release(
            200,
            serde_json::json!({
                "tag_name": "v1.4.2",
                "html_url": "https://github.com/x/y/releases/tag/v1.4.2",
                "draft": false,
                "prerelease": false,
            }),
        )
        .await;
        let result = check_with(&server.uri(), "1.4.1").await.unwrap();
        let info = result.expect("expected Some(UpdateInfo)");
        assert_eq!(info.version, "1.4.2");
        assert_eq!(info.url, "https://github.com/x/y/releases/tag/v1.4.2");
    }

    #[tokio::test]
    async fn same_tag_returns_none() {
        let server = mock_latest_release(
            200,
            serde_json::json!({
                "tag_name": "v0.3.0",
                "html_url": "https://example/r",
                "draft": false,
                "prerelease": false,
            }),
        )
        .await;
        assert_eq!(check_with(&server.uri(), "0.3.0").await.unwrap(), None);
    }

    #[tokio::test]
    async fn draft_release_returns_none_even_if_newer() {
        let server = mock_latest_release(
            200,
            serde_json::json!({
                "tag_name": "v9.9.9",
                "html_url": "https://example/r",
                "draft": true,
                "prerelease": false,
            }),
        )
        .await;
        assert_eq!(check_with(&server.uri(), "0.1.0").await.unwrap(), None);
    }

    #[tokio::test]
    async fn prerelease_returns_none_even_if_newer() {
        let server = mock_latest_release(
            200,
            serde_json::json!({
                "tag_name": "v9.9.9",
                "html_url": "https://example/r",
                "draft": false,
                "prerelease": true,
            }),
        )
        .await;
        assert_eq!(check_with(&server.uri(), "0.1.0").await.unwrap(), None);
    }

    #[tokio::test]
    async fn non_200_response_returns_err() {
        let server = mock_latest_release(503, serde_json::json!({})).await;
        let err = check_with(&server.uri(), "0.1.0").await.unwrap_err();
        // Surface the upstream status in the error so logs are useful.
        assert!(
            err.to_string().contains("503"),
            "expected status in error: {err}"
        );
    }

    #[tokio::test]
    async fn unparseable_tag_returns_none() {
        // "v-not-semver" deserialises into the struct fine but
        // semver::Version::parse fails — that's the "GitHub published
        // a non-semver tag" branch.
        let server = mock_latest_release(
            200,
            serde_json::json!({
                "tag_name": "v-not-semver",
                "html_url": "https://example/r",
                "draft": false,
                "prerelease": false,
            }),
        )
        .await;
        assert_eq!(check_with(&server.uri(), "0.1.0").await.unwrap(), None);
    }

    #[tokio::test]
    async fn malformed_body_returns_err() {
        // Body is JSON but missing required fields — serde_json bubbles
        // up as a `.json()` decoding error from reqwest.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(format!("/repos/{}/releases/latest", REPO)))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"unrelated": true})),
            )
            .mount(&server)
            .await;
        assert!(check_with(&server.uri(), "0.1.0").await.is_err());
    }

    #[tokio::test]
    async fn unparseable_current_version_returns_none() {
        let server = mock_latest_release(
            200,
            serde_json::json!({
                "tag_name": "v1.0.0",
                "html_url": "https://example/r",
                "draft": false,
                "prerelease": false,
            }),
        )
        .await;
        // "dev" is not semver — we accept the running binary's version
        // as authoritative and just refuse to compare.
        assert_eq!(check_with(&server.uri(), "dev").await.unwrap(), None);
    }

    #[tokio::test]
    async fn trailing_slash_in_base_url_is_tolerated() {
        let server = mock_latest_release(
            200,
            serde_json::json!({
                "tag_name": "v2.0.0",
                "html_url": "https://example/r",
                "draft": false,
                "prerelease": false,
            }),
        )
        .await;
        let with_slash = format!("{}/", server.uri());
        let info = check_with(&with_slash, "1.0.0").await.unwrap().unwrap();
        assert_eq!(info.version, "2.0.0");
    }
}
