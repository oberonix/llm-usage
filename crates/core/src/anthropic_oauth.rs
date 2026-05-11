//! Polls Anthropic's internal `/api/oauth/usage` endpoint using the OAuth
//! token Claude Code has already obtained. This is the same endpoint Claude
//! Code itself uses to render its `/usage` view, so the data matches what the
//! user sees in their statusline.
//!
//! Endpoint:  GET https://api.anthropic.com/api/oauth/usage
//! Auth:      Authorization: Bearer <accessToken>
//! Beta hdr:  anthropic-beta: oauth-2025-04-20
//! Token at:  ~/.claude/.credentials.json  → claudeAiOauth.accessToken
//!
//! Token refresh is intentionally not implemented here. If the token has
//! expired we surface a clear error so the user knows to re-authenticate
//! via Claude Code itself.

use chrono::{DateTime, Utc};
use serde::Deserialize;
use std::path::PathBuf;
use thiserror::Error;

const OAUTH_USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
const OAUTH_BETA_HEADER: &str = "oauth-2025-04-20";

#[derive(Debug, Error)]
pub enum OAuthError {
    #[error("missing or unreadable credentials: {0}")]
    Credentials(String),
    #[error("Claude Code OAuth token expired — re-authenticate via Claude Code")]
    Expired,
    #[error("rate limited by Anthropic (HTTP 429)")]
    RateLimited,
    #[error("HTTP {0}")]
    Http(reqwest::StatusCode),
    #[error(transparent)]
    Transport(#[from] reqwest::Error),
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

impl OAuthError {
    pub fn is_rate_limited(&self) -> bool {
        matches!(self, OAuthError::RateLimited)
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct OAuthCredentials {
    #[serde(rename = "claudeAiOauth")]
    pub claude_ai_oauth: OAuthBody,
}

#[derive(Debug, Clone, Deserialize)]
pub struct OAuthBody {
    #[serde(rename = "accessToken")]
    pub access_token: String,
    #[serde(rename = "expiresAt", default)]
    pub expires_at_ms: Option<i64>,
    #[serde(rename = "subscriptionType", default)]
    pub subscription_type: Option<String>,
    #[serde(rename = "rateLimitTier", default)]
    pub rate_limit_tier: Option<String>,
}

impl OAuthCredentials {
    pub fn load() -> Result<Self, OAuthError> {
        let path = credentials_path()?;
        let s = std::fs::read_to_string(&path)
            .map_err(|e| OAuthError::Credentials(format!("read {}: {}", path.display(), e)))?;
        let creds: OAuthCredentials = serde_json::from_str(&s)
            .map_err(|e| OAuthError::Credentials(format!("parse {}: {}", path.display(), e)))?;
        Ok(creds)
    }

    pub fn is_expired(&self) -> bool {
        match self.claude_ai_oauth.expires_at_ms {
            Some(ms) => Utc::now().timestamp_millis() >= ms,
            None => false,
        }
    }
}

pub fn credentials_path() -> Result<PathBuf, OAuthError> {
    Ok(dirs::home_dir()
        .ok_or_else(|| OAuthError::Credentials("no home dir".into()))?
        .join(".claude")
        .join(".credentials.json"))
}

/// One quota bucket as returned by the endpoint.
/// `utilization` is 0..100 (percent used).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct QuotaBucket {
    pub utilization: f64,
    pub resets_at: Option<String>,
}

impl QuotaBucket {
    pub fn resets_at_utc(&self) -> Option<DateTime<Utc>> {
        self.resets_at
            .as_deref()
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok().map(|d| d.with_timezone(&Utc)))
    }
}

/// Subset of the response we surface in the tray. Other fields exist
/// (`seven_day_oauth_apps`, `seven_day_cowork`, `seven_day_omelette`, etc.)
/// but are codenamed and not generally relevant.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct OAuthUsageResponse {
    pub five_hour: Option<QuotaBucket>,
    pub seven_day: Option<QuotaBucket>,
    pub seven_day_sonnet: Option<QuotaBucket>,
    pub seven_day_opus: Option<QuotaBucket>,
    pub extra_usage: Option<ExtraUsage>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ExtraUsage {
    pub is_enabled: Option<bool>,
    pub monthly_limit: Option<f64>,
    pub used_credits: Option<f64>,
    pub utilization: Option<f64>,
    pub currency: Option<String>,
}

pub async fn fetch_usage(client: &reqwest::Client) -> Result<OAuthUsageResponse, OAuthError> {
    let creds = OAuthCredentials::load()?;
    if creds.is_expired() {
        return Err(OAuthError::Expired);
    }
    let token = creds.claude_ai_oauth.access_token;
    let resp = client
        .get(OAUTH_USAGE_URL)
        .bearer_auth(token)
        .header("anthropic-beta", OAUTH_BETA_HEADER)
        .header("content-type", "application/json")
        .send()
        .await?;
    let status = resp.status();
    if status.as_u16() == 429 {
        return Err(OAuthError::RateLimited);
    }
    if !status.is_success() {
        return Err(OAuthError::Http(status));
    }
    let body: OAuthUsageResponse = resp.json().await.map_err(OAuthError::Transport)?;
    Ok(body)
}

/// Backoff state shared across polls. After a 429, hold off calling the
/// OAuth endpoint until `next_allowed_unix_secs`. On repeated 429s, double
/// the cooldown up to 30 minutes. On the first success after a backoff,
/// reset the multiplier.
#[derive(Debug, Default)]
pub struct OAuthBackoff {
    pub next_allowed_unix_secs: i64,
    pub current_cooldown_secs: i64,
    /// Cached last-good response, surfaced during cooldown so the tray keeps
    /// showing useful (if stale) numbers.
    pub last_good: Option<OAuthUsageResponse>,
    pub last_good_at: Option<DateTime<Utc>>,
}

impl OAuthBackoff {
    /// Initial cooldown after the first 429 (5 min). Doubles up to 30 min.
    pub const INITIAL_COOLDOWN_SECS: i64 = 5 * 60;
    pub const MAX_COOLDOWN_SECS: i64 = 30 * 60;

    pub fn in_cooldown(&self, now: DateTime<Utc>) -> bool {
        now.timestamp() < self.next_allowed_unix_secs
    }

    pub fn cooldown_remaining(&self, now: DateTime<Utc>) -> i64 {
        (self.next_allowed_unix_secs - now.timestamp()).max(0)
    }

    pub fn record_429(&mut self, now: DateTime<Utc>) {
        let next = if self.current_cooldown_secs == 0 {
            Self::INITIAL_COOLDOWN_SECS
        } else {
            (self.current_cooldown_secs * 2).min(Self::MAX_COOLDOWN_SECS)
        };
        self.current_cooldown_secs = next;
        self.next_allowed_unix_secs = now.timestamp() + next;
    }

    pub fn record_success(&mut self, now: DateTime<Utc>, body: &OAuthUsageResponse) {
        self.next_allowed_unix_secs = 0;
        self.current_cooldown_secs = 0;
        self.last_good = Some(body.clone());
        self.last_good_at = Some(now);
    }
}
