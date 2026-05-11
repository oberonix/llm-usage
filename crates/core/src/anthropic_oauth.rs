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

    /// Human-readable plan label derived from the credentials file.
    /// We prefer `rateLimitTier` because it carries the multiplier
    /// (`default_claude_max_5x`, `default_claude_max_20x`); fall back to
    /// `subscriptionType` for tiers that don't have one (e.g. "pro",
    /// "team").
    ///
    /// Examples:
    /// - `rateLimitTier="default_claude_max_5x"` → `Some("Max 5x")`
    /// - `subscriptionType="pro"`                → `Some("Pro")`
    pub fn plan_label(&self) -> Option<String> {
        let body = &self.claude_ai_oauth;
        if let Some(tier) = body.rate_limit_tier.as_deref() {
            let trimmed = tier.strip_prefix("default_claude_").unwrap_or(tier);
            if let Some(rest) = trimmed.strip_prefix("max_") {
                return Some(format!("Max {}", rest));
            }
        }
        body.subscription_type
            .as_deref()
            .map(crate::model::title_case_first)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn creds(sub: Option<&str>, tier: Option<&str>) -> OAuthCredentials {
        OAuthCredentials {
            claude_ai_oauth: OAuthBody {
                access_token: "tok".into(),
                expires_at_ms: None,
                subscription_type: sub.map(String::from),
                rate_limit_tier: tier.map(String::from),
            },
        }
    }

    #[test]
    fn plan_label_max_with_multiplier() {
        let c = creds(Some("max"), Some("default_claude_max_5x"));
        assert_eq!(c.plan_label().as_deref(), Some("Max 5x"));
        let c = creds(Some("max"), Some("default_claude_max_20x"));
        assert_eq!(c.plan_label().as_deref(), Some("Max 20x"));
    }

    #[test]
    fn plan_label_falls_back_to_subscription_for_pro() {
        let c = creds(Some("pro"), Some("default_claude_pro"));
        assert_eq!(c.plan_label().as_deref(), Some("Pro"));
    }

    #[test]
    fn plan_label_handles_team_via_subscription() {
        // Hypothetical "team" tier — exercise the fall-through path.
        let c = creds(Some("team"), Some("default_claude_team"));
        assert_eq!(c.plan_label().as_deref(), Some("Team"));
    }

    #[test]
    fn plan_label_none_when_no_data() {
        let c = creds(None, None);
        assert!(c.plan_label().is_none());
    }

    #[test]
    fn plan_label_strips_default_claude_prefix_only_when_present() {
        // Unknown tier without the prefix — falls through to subscription.
        let c = creds(Some("max"), Some("weird-tier-no-prefix"));
        assert_eq!(c.plan_label().as_deref(), Some("Max"));
    }

    #[test]
    fn is_expired_relative_to_now() {
        let past_ms = (Utc::now() - chrono::Duration::hours(1)).timestamp_millis();
        let future_ms = (Utc::now() + chrono::Duration::hours(1)).timestamp_millis();
        let expired = OAuthCredentials {
            claude_ai_oauth: OAuthBody {
                access_token: "tok".into(),
                expires_at_ms: Some(past_ms),
                subscription_type: None,
                rate_limit_tier: None,
            },
        };
        let live = OAuthCredentials {
            claude_ai_oauth: OAuthBody {
                access_token: "tok".into(),
                expires_at_ms: Some(future_ms),
                subscription_type: None,
                rate_limit_tier: None,
            },
        };
        assert!(expired.is_expired());
        assert!(!live.is_expired());
    }

    #[test]
    fn is_expired_treats_missing_expiry_as_alive() {
        let c = OAuthCredentials {
            claude_ai_oauth: OAuthBody {
                access_token: "tok".into(),
                expires_at_ms: None,
                subscription_type: None,
                rate_limit_tier: None,
            },
        };
        assert!(!c.is_expired());
    }

    #[test]
    fn quota_bucket_parses_iso_resets_at() {
        let b = QuotaBucket {
            utilization: 12.0,
            resets_at: Some("2026-05-10T03:00:00Z".to_string()),
        };
        let t = b.resets_at_utc().unwrap();
        // Confirm the parse hit Z time and rebuild the same ISO string.
        assert_eq!(t.to_rfc3339(), "2026-05-10T03:00:00+00:00");
    }

    #[test]
    fn quota_bucket_returns_none_on_bad_iso() {
        let b = QuotaBucket {
            utilization: 0.0,
            resets_at: Some("not a date".into()),
        };
        assert!(b.resets_at_utc().is_none());
    }

    #[test]
    fn backoff_first_429_uses_initial_cooldown() {
        let mut b = OAuthBackoff::default();
        let now = Utc::now();
        b.record_429(now);
        assert_eq!(b.current_cooldown_secs, OAuthBackoff::INITIAL_COOLDOWN_SECS);
        assert!(b.in_cooldown(now));
        // Just past the cooldown deadline — no longer in cooldown.
        let later = now + chrono::Duration::seconds(b.current_cooldown_secs + 1);
        assert!(!b.in_cooldown(later));
    }

    #[test]
    fn backoff_doubles_up_to_cap_on_repeated_429s() {
        let mut b = OAuthBackoff::default();
        let now = Utc::now();
        b.record_429(now);
        let first = b.current_cooldown_secs;
        b.record_429(now);
        assert_eq!(b.current_cooldown_secs, (first * 2).min(OAuthBackoff::MAX_COOLDOWN_SECS));
        // Force enough repeats to hit the cap.
        for _ in 0..10 {
            b.record_429(now);
        }
        assert_eq!(b.current_cooldown_secs, OAuthBackoff::MAX_COOLDOWN_SECS);
    }

    #[test]
    fn backoff_record_success_resets_state() {
        let mut b = OAuthBackoff::default();
        let now = Utc::now();
        b.record_429(now);
        b.record_429(now);
        assert!(b.current_cooldown_secs > 0);

        let body = OAuthUsageResponse::default();
        b.record_success(now, &body);
        assert_eq!(b.current_cooldown_secs, 0);
        assert_eq!(b.next_allowed_unix_secs, 0);
        assert!(b.last_good.is_some());
        assert!(!b.in_cooldown(now));
    }

    #[test]
    fn backoff_cooldown_remaining_is_zero_after_expiry() {
        let mut b = OAuthBackoff::default();
        let now = Utc::now();
        b.record_429(now);
        let later = now + chrono::Duration::seconds(b.current_cooldown_secs + 60);
        assert_eq!(b.cooldown_remaining(later), 0);
    }
}
