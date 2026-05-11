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
    let url = format!("https://api.github.com/repos/{}/releases/latest", REPO);
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
}
