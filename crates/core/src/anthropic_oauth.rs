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
        if let Ok(path) = credentials_path() {
            if path.exists() {
                if let Ok(creds) = Self::load_from_file(&path) {
                    return Ok(creds);
                }
            }
        }
        #[cfg(target_os = "macos")]
        {
            if let Ok(creds) = Self::load_from_keychain() {
                return Ok(creds);
            }
        }
        Err(OAuthError::Credentials(
            "no credentials found (checked ~/.claude/.credentials.json and system keychain)"
                .into(),
        ))
    }

    fn load_from_file(path: &std::path::Path) -> Result<Self, OAuthError> {
        let s = std::fs::read_to_string(path)
            .map_err(|e| OAuthError::Credentials(format!("read {}: {}", path.display(), e)))?;
        let creds: OAuthCredentials = serde_json::from_str(&s)
            .map_err(|e| OAuthError::Credentials(format!("parse {}: {}", path.display(), e)))?;
        Ok(creds)
    }

    #[cfg(target_os = "macos")]
    fn load_from_keychain() -> Result<Self, OAuthError> {
        let output = std::process::Command::new("security")
            .args([
                "find-generic-password",
                "-s",
                "Claude Code-credentials",
                "-w",
            ])
            .output()
            .map_err(|e| OAuthError::Credentials(format!("keychain command: {}", e)))?;
        if !output.status.success() {
            return Err(OAuthError::Credentials(
                "Claude Code-credentials not found in keychain".into(),
            ));
        }
        let s = String::from_utf8_lossy(&output.stdout);
        let creds: OAuthCredentials = serde_json::from_str(s.trim())
            .map_err(|e| OAuthError::Credentials(format!("parse keychain: {}", e)))?;
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
        self.resets_at.as_deref().and_then(|s| {
            DateTime::parse_from_rfc3339(s)
                .ok()
                .map(|d| d.with_timezone(&Utc))
        })
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
    fetch_usage_with(client, OAUTH_USAGE_URL, &creds).await
}

/// Same as [`fetch_usage`] but with the URL and credentials supplied
/// explicitly. The `fetch_usage` shim reads credentials from disk; this
/// variant lets tests (and any future caller that already has fresh
/// credentials in memory) skip the file IO and target a stub server.
pub async fn fetch_usage_with(
    client: &reqwest::Client,
    url: &str,
    creds: &OAuthCredentials,
) -> Result<OAuthUsageResponse, OAuthError> {
    if creds.is_expired() {
        return Err(OAuthError::Expired);
    }
    let token = creds.claude_ai_oauth.access_token.clone();
    let resp = client
        .get(url)
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

    /// True when the caller should *skip* a fresh HTTP call and reuse
    /// `last_good`. Two cases combined:
    ///
    ///   1. We're inside the 429 cooldown window (existing behaviour).
    ///   2. The last successful response is younger than `min_interval`
    ///      — covers file-watcher-driven refresh storms where local
    ///      JSONL writes could otherwise trigger many `/usage` calls
    ///      per minute and surprise-429 us.
    ///
    /// Returns `false` when no `last_good` has ever been recorded so
    /// the first poll after startup is always allowed through.
    pub fn should_skip_http(&self, now: DateTime<Utc>, min_interval: std::time::Duration) -> bool {
        if self.in_cooldown(now) {
            return true;
        }
        match self.last_good_at {
            Some(t) => match (now - t).to_std() {
                Ok(elapsed) => elapsed < min_interval,
                // Clock skew → treat as "skip" defensively; we don't
                // want to fire HTTP based on a wonky time delta.
                Err(_) => true,
            },
            None => false,
        }
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
        assert_eq!(
            b.current_cooldown_secs,
            (first * 2).min(OAuthBackoff::MAX_COOLDOWN_SECS)
        );
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
    fn should_skip_http_returns_false_when_no_prior_call() {
        // Cold start: never made an HTTP call yet → always allowed.
        let b = OAuthBackoff::default();
        assert!(!b.should_skip_http(Utc::now(), std::time::Duration::from_secs(60)));
    }

    #[test]
    fn should_skip_http_throttles_within_min_interval() {
        // last_good_at = 30s ago, min_interval = 60s → throttled.
        let mut b = OAuthBackoff::default();
        let now = Utc::now();
        b.record_success(
            now - chrono::Duration::seconds(30),
            &OAuthUsageResponse::default(),
        );
        assert!(b.should_skip_http(now, std::time::Duration::from_secs(60)));
        // Outside the interval → not throttled.
        assert!(!b.should_skip_http(now, std::time::Duration::from_secs(10)));
    }

    #[test]
    fn should_skip_http_returns_true_during_429_cooldown_even_if_min_interval_elapsed() {
        // 429 cooldown wins over min-interval check — we must NOT
        // hammer a rate-limited endpoint just because the soft
        // throttle says it's been long enough since last success.
        let mut b = OAuthBackoff::default();
        let now = Utc::now();
        b.record_success(
            now - chrono::Duration::hours(1),
            &OAuthUsageResponse::default(),
        );
        b.record_429(now); // installs the 5min cooldown
        assert!(b.should_skip_http(now, std::time::Duration::from_secs(60)));
    }

    #[test]
    fn backoff_cooldown_remaining_is_zero_after_expiry() {
        let mut b = OAuthBackoff::default();
        let now = Utc::now();
        b.record_429(now);
        let later = now + chrono::Duration::seconds(b.current_cooldown_secs + 60);
        assert_eq!(b.cooldown_remaining(later), 0);
    }

    #[test]
    fn is_rate_limited_only_true_for_that_variant() {
        let e: OAuthError = OAuthError::RateLimited;
        assert!(e.is_rate_limited());
        let other: OAuthError = OAuthError::Expired;
        assert!(!other.is_rate_limited());
    }

    #[test]
    fn credentials_path_includes_dot_claude_dot_credentials_json() {
        // We don't assert the exact home path (varies per-machine) but
        // the trailing two components must match what Claude Code
        // writes to. A typo here is a silent "couldn't read
        // credentials" at runtime.
        let p = credentials_path().expect("credentials_path");
        let s = p.to_string_lossy();
        assert!(s.ends_with(".claude/.credentials.json"), "got: {s}");
    }

    // ---- HTTP-layer tests against wiremock ----

    use wiremock::matchers::{header, method};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn fresh_creds() -> OAuthCredentials {
        OAuthCredentials {
            claude_ai_oauth: OAuthBody {
                access_token: "test-token".into(),
                // Not expired — well in the future.
                expires_at_ms: Some(
                    (chrono::Utc::now() + chrono::Duration::hours(24)).timestamp_millis(),
                ),
                subscription_type: Some("max".into()),
                rate_limit_tier: Some("default_claude_max_5x".into()),
            },
        }
    }

    #[tokio::test]
    async fn fetch_usage_parses_full_response() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(header("authorization", "Bearer test-token"))
            .and(header("anthropic-beta", OAUTH_BETA_HEADER))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "five_hour": {"utilization": 55.0, "resets_at": "2026-05-12T05:00:00Z"},
                "seven_day": {"utilization": 58.0, "resets_at": "2026-05-13T15:00:00Z"},
                "seven_day_opus": null,
                "seven_day_sonnet": {"utilization": 31.0, "resets_at": "2026-05-13T15:00:00Z"},
                "extra_usage": {
                    "is_enabled": true,
                    "monthly_limit": 100.0,
                    "used_credits": 12.34,
                    "utilization": 12.34,
                    "currency": "USD"
                }
            })))
            .expect(1)
            .mount(&server)
            .await;

        let client = reqwest::Client::new();
        let body = fetch_usage_with(&client, &server.uri(), &fresh_creds())
            .await
            .unwrap();
        assert!((body.five_hour.unwrap().utilization - 55.0).abs() < 1e-6);
        assert!((body.seven_day.unwrap().utilization - 58.0).abs() < 1e-6);
        assert!(body.seven_day_opus.is_none());
        assert!((body.seven_day_sonnet.unwrap().utilization - 31.0).abs() < 1e-6);
        assert_eq!(body.extra_usage.unwrap().currency.as_deref(), Some("USD"));
    }

    #[tokio::test]
    async fn fetch_usage_handles_missing_optional_buckets() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
            .mount(&server)
            .await;
        let body = fetch_usage_with(&reqwest::Client::new(), &server.uri(), &fresh_creds())
            .await
            .unwrap();
        assert!(body.five_hour.is_none());
        assert!(body.seven_day.is_none());
    }

    #[tokio::test]
    async fn fetch_usage_429_returns_rate_limited_variant() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(429))
            .mount(&server)
            .await;
        let err = fetch_usage_with(&reqwest::Client::new(), &server.uri(), &fresh_creds())
            .await
            .unwrap_err();
        assert!(err.is_rate_limited(), "got: {err}");
    }

    #[tokio::test]
    async fn fetch_usage_500_returns_http_variant() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;
        let err = fetch_usage_with(&reqwest::Client::new(), &server.uri(), &fresh_creds())
            .await
            .unwrap_err();
        match err {
            OAuthError::Http(code) => assert_eq!(code.as_u16(), 503),
            other => panic!("expected Http variant; got {other:?}"),
        }
    }

    #[tokio::test]
    async fn fetch_usage_short_circuits_when_creds_expired() {
        let server = MockServer::start().await;
        // No mock registered: if we made the request the test would
        // produce an "unexpected request" warning.
        let mut creds = fresh_creds();
        creds.claude_ai_oauth.expires_at_ms =
            Some((chrono::Utc::now() - chrono::Duration::hours(1)).timestamp_millis());
        let err = fetch_usage_with(&reqwest::Client::new(), &server.uri(), &creds)
            .await
            .unwrap_err();
        assert!(matches!(err, OAuthError::Expired));
    }

    #[tokio::test]
    async fn fetch_usage_malformed_body_returns_transport_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_string("definitely not json"))
            .mount(&server)
            .await;
        let err = fetch_usage_with(&reqwest::Client::new(), &server.uri(), &fresh_creds())
            .await
            .unwrap_err();
        // The reqwest `.json()` failure surfaces as Transport.
        assert!(matches!(err, OAuthError::Transport(_)));
    }
}
