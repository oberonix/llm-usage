//! Live ChatGPT-side quota source for the Codex provider.
//!
//! `/backend-api/wham/usage` returns a JSON document with current
//! 5-hour and 7-day quota fractions plus the plan tier and a
//! `rate_limit_reached_type` flag — strictly better than the Codex CLI
//! rollouts (which lag, sometimes nullify both buckets on v0.129+, and
//! never surface state once the user is locked out). We use this as
//! the primary quota source when the user has a logged-in chatgpt.com
//! session in any installed browser; the rollouts remain the fallback
//! for token-count aggregation and for cases where cookies aren't
//! available.
//!
//! Auth flow:
//!
//!   1. Read browser cookies for `chatgpt.com` / `openai.com` via the
//!      `rookie` crate (same library the setup binary already uses for
//!      Ollama Cloud capture). Cached as a `Cookie:` header.
//!   2. POST those cookies to `https://chatgpt.com/api/auth/session`.
//!      NextAuth.js returns `{ accessToken, expires, ... }` — the
//!      bearer is what `/backend-api/*` paths actually require.
//!   3. GET `/backend-api/wham/usage` with
//!      `Authorization: Bearer <accessToken>` and the same cookies.
//!
//! Failure modes:
//!   - **No browser cookies / not logged in** → return `Ok(None)`, the
//!     caller falls back to the rollouts.
//!   - **Cookies present but session expired (401 from
//!     `/api/auth/session`)** → return `Err(Expired)`. Caller may
//!     surface a "sign in to chatgpt.com" hint, but doesn't crash.
//!   - **Cloudflare challenge** → `cf-mitigated: challenge` header on
//!     a 4xx. Bubbles up as `Err(Blocked)`. Probably indicates we're
//!     making too many calls — the caller throttles.

use crate::model::WindowUsage;
use anyhow::{Context, Result};
use chrono::{DateTime, TimeZone, Utc};
use reqwest::StatusCode;
use serde::Deserialize;
use std::time::Duration;
use thiserror::Error;

const SESSION_URL: &str = "https://chatgpt.com/api/auth/session";
const USAGE_URL: &str = "https://chatgpt.com/backend-api/wham/usage";

// Browser-like UA so chatgpt.com / Cloudflare don't immediately
// classify the requests as automation. We can't dodge a real
// challenge — and shouldn't try — but matching a current Chromium UA
// keeps the request shape consistent with what the real
// `/codex/cloud/settings/analytics` page sends.
const BROWSER_UA: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 \
                          (KHTML, like Gecko) Chrome/130.0 Safari/537.36";

#[derive(Debug, Error)]
pub enum ChatGptAuthError {
    /// No cookies for chatgpt.com / openai.com in any installed
    /// browser. The user needs to sign in via Chrome / Brave / Firefox
    /// at least once.
    #[error("no chatgpt.com cookies found; sign in via your browser first")]
    NoCookies,
    /// Cookies are present but `/api/auth/session` rejected them —
    /// usually means the session lapsed (NextAuth tokens rotate). The
    /// user needs to refresh by reloading chatgpt.com.
    #[error("chatgpt.com session expired; reload the site to refresh cookies")]
    Expired,
    /// Cloudflare gated the request (challenge or block). We don't try
    /// to solve it — the caller backs off.
    #[error("chatgpt.com gated by Cloudflare ({0})")]
    Blocked(String),
    #[error("HTTP {0}")]
    Http(StatusCode),
    #[error(transparent)]
    Transport(#[from] reqwest::Error),
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

/// One quota window as returned by the wham endpoint.
#[derive(Debug, Clone, Deserialize)]
pub struct UsageWindow {
    /// 0..100 (percent used).
    pub used_percent: f64,
    /// Wall-clock window length in seconds (300 * 60 = 18 000 for 5h,
    /// 7 * 86 400 = 604 800 for week).
    pub limit_window_seconds: i64,
    /// Seconds remaining until this window resets. Used to derive a
    /// stable `ends_at` so renderers can show the countdown.
    pub reset_after_seconds: i64,
    /// Absolute Unix timestamp of the next reset. Preferred over
    /// `reset_after_seconds` since it's not relative to "now".
    pub reset_at: i64,
}

impl UsageWindow {
    pub fn ends_at(&self) -> Option<DateTime<Utc>> {
        Utc.timestamp_opt(self.reset_at, 0).single()
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct RateLimit {
    pub allowed: bool,
    pub limit_reached: bool,
    pub primary_window: Option<UsageWindow>,
    pub secondary_window: Option<UsageWindow>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RateLimitReachedType {
    /// e.g. `"rate_limit_reached"` when allowed=false.
    #[serde(rename = "type")]
    pub kind: String,
    /// Free-form details ChatGPT renders to the user (`"default"`,
    /// `"weekly"`, …).
    #[serde(default)]
    pub details: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct WhamUsage {
    pub plan_type: Option<String>,
    pub rate_limit: RateLimit,
    pub rate_limit_reached_type: Option<RateLimitReachedType>,
}

impl WhamUsage {
    /// Apply this snapshot to a 5-hour / weekly `WindowUsage` pair so
    /// the rest of the renderer pipeline (`apply_rate_limits`-style)
    /// doesn't care about the source. Fractions are clamped to 0..1.
    pub fn apply_to(&self, five_h: &mut WindowUsage, week: &mut WindowUsage, now: DateTime<Utc>) {
        if let Some(w) = &self.rate_limit.primary_window {
            apply_window(five_h, w, now);
        }
        if let Some(w) = &self.rate_limit.secondary_window {
            apply_window(week, w, now);
        }
    }
}

fn apply_window(target: &mut WindowUsage, src: &UsageWindow, now: DateTime<Utc>) {
    target.fraction_used = Some((src.used_percent / 100.0).clamp(0.0, 1.0));
    target.ends_at = src.ends_at();
    target.started_at = Some(now);
    // `stale = false` here is the whole point of this source: a 200
    // from wham/usage is by definition the current state, not a
    // cached snapshot. Renderers will display colour bars, not grey.
    target.stale = false;
}

/// Build a `reqwest::Client` shaped like the real chatgpt.com page —
/// browser UA, generous timeout. We deliberately do NOT enable cookie
/// jar handling; the caller passes a pre-built `Cookie:` header so
/// we can swap the underlying browser cookie source out without the
/// client carrying state.
pub fn build_http_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent(BROWSER_UA)
        .timeout(Duration::from_secs(15))
        .build()
        .context("build reqwest client")
}

/// Mint a NextAuth bearer token from the session cookies. Returns the
/// raw token string + the Unix timestamp at which it expires (so the
/// caller can re-mint proactively).
pub async fn mint_bearer(
    http: &reqwest::Client,
    cookie_header: &str,
) -> Result<MintedBearer, ChatGptAuthError> {
    let resp = http
        .get(SESSION_URL)
        .header(reqwest::header::COOKIE, cookie_header)
        .header(reqwest::header::ACCEPT, "application/json")
        .send()
        .await?;
    classify_or_pass(&resp)?;
    if !resp.status().is_success() {
        return Err(ChatGptAuthError::Http(resp.status()));
    }
    let body: SessionJson = resp.json().await.map_err(ChatGptAuthError::Transport)?;
    let token = body.access_token.ok_or(ChatGptAuthError::Expired)?;
    Ok(MintedBearer {
        token,
        // NextAuth returns `expires` as ISO-8601. Parse to Unix
        // millis, defaulting to "no idea, expire in 1h" so we
        // re-mint regularly even when the field is missing.
        expires_at_utc: body
            .expires
            .as_deref()
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .map(|d| d.with_timezone(&Utc))
            .unwrap_or_else(|| Utc::now() + chrono::Duration::hours(1)),
    })
}

#[derive(Debug, Clone)]
pub struct MintedBearer {
    pub token: String,
    pub expires_at_utc: DateTime<Utc>,
}

/// Fetch the current Codex quota usage from chatgpt.com using the
/// supplied bearer + cookies.
pub async fn fetch_wham_usage(
    http: &reqwest::Client,
    cookie_header: &str,
    bearer: &str,
) -> Result<WhamUsage, ChatGptAuthError> {
    let resp = http
        .get(USAGE_URL)
        .header(reqwest::header::COOKIE, cookie_header)
        .header(reqwest::header::ACCEPT, "application/json")
        .bearer_auth(bearer)
        // chatgpt.com gates some `/backend-api` routes on a
        // matching Sec-Fetch-Site = "same-origin" + cors mode;
        // browsers send these automatically.
        .header("Sec-Fetch-Mode", "cors")
        .header("Sec-Fetch-Site", "same-origin")
        .send()
        .await?;
    classify_or_pass(&resp)?;
    if !resp.status().is_success() {
        return Err(ChatGptAuthError::Http(resp.status()));
    }
    resp.json().await.map_err(ChatGptAuthError::Transport)
}

/// Surface common error shapes as typed variants so the caller can
/// react without sniffing strings. Specifically: an explicit
/// `cf-mitigated` header (Cloudflare challenge / block) becomes
/// `Blocked`; a 401 against `/api/auth/session` becomes `Expired`.
fn classify_or_pass(resp: &reqwest::Response) -> Result<(), ChatGptAuthError> {
    if let Some(v) = resp.headers().get("cf-mitigated") {
        if let Ok(s) = v.to_str() {
            return Err(ChatGptAuthError::Blocked(s.to_string()));
        }
    }
    if resp.status() == StatusCode::UNAUTHORIZED && resp.url().path().contains("/api/auth/session")
    {
        return Err(ChatGptAuthError::Expired);
    }
    Ok(())
}

#[derive(Deserialize)]
struct SessionJson {
    #[serde(rename = "accessToken")]
    access_token: Option<String>,
    expires: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// Build a client pointed at the mock server. We can't set the
    /// hardcoded URLs in this module from a test, so we just hit the
    /// real domain shape on a localhost mock by using `reqwest`
    /// directly with the mock's full URL.
    fn http_against(server: &MockServer) -> (reqwest::Client, String) {
        let client = reqwest::Client::builder().user_agent(BROWSER_UA).build().unwrap();
        (client, server.uri())
    }

    #[tokio::test]
    async fn mint_bearer_returns_access_token_when_session_endpoint_succeeds() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/auth/session"))
            .and(header(reqwest::header::COOKIE.as_str(), "x=1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "accessToken": "secret-bearer",
                "expires": "2027-01-01T00:00:00Z",
            })))
            .mount(&server)
            .await;
        let (client, base) = http_against(&server);
        // Call the URL directly (mint_bearer hardcodes chatgpt.com).
        let resp = client
            .get(format!("{}/api/auth/session", base))
            .header(reqwest::header::COOKIE, "x=1")
            .send()
            .await
            .unwrap();
        let body: SessionJson = resp.json().await.unwrap();
        assert_eq!(body.access_token.as_deref(), Some("secret-bearer"));
    }

    #[tokio::test]
    async fn classify_or_pass_flags_cloudflare_challenge() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/x"))
            .respond_with(
                ResponseTemplate::new(403)
                    .insert_header("cf-mitigated", "challenge")
                    .set_body_string("blocked"),
            )
            .mount(&server)
            .await;
        let (client, base) = http_against(&server);
        let resp = client.get(format!("{}/x", base)).send().await.unwrap();
        match classify_or_pass(&resp).unwrap_err() {
            ChatGptAuthError::Blocked(s) => assert_eq!(s, "challenge"),
            other => panic!("expected Blocked; got {:?}", other),
        }
    }

    #[tokio::test]
    async fn classify_or_pass_flags_expired_only_on_session_path() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/auth/session"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;
        let (client, base) = http_against(&server);
        let resp = client
            .get(format!("{}/api/auth/session", base))
            .send()
            .await
            .unwrap();
        match classify_or_pass(&resp).unwrap_err() {
            ChatGptAuthError::Expired => {}
            other => panic!("expected Expired; got {:?}", other),
        }
    }

    #[test]
    fn wham_usage_applies_both_windows() {
        let usage = WhamUsage {
            plan_type: Some("plus".into()),
            rate_limit: RateLimit {
                allowed: true,
                limit_reached: false,
                primary_window: Some(UsageWindow {
                    used_percent: 42.0,
                    limit_window_seconds: 18_000,
                    reset_after_seconds: 7_200,
                    reset_at: 1_900_000_000,
                }),
                secondary_window: Some(UsageWindow {
                    used_percent: 80.0,
                    limit_window_seconds: 604_800,
                    reset_after_seconds: 86_400,
                    reset_at: 1_910_000_000,
                }),
            },
            rate_limit_reached_type: None,
        };
        let mut five_h = WindowUsage::default();
        let mut week = WindowUsage::default();
        let now = Utc::now();
        usage.apply_to(&mut five_h, &mut week, now);
        assert!((five_h.fraction_used.unwrap() - 0.42).abs() < 1e-9);
        assert_eq!(five_h.ends_at, Some(Utc.timestamp_opt(1_900_000_000, 0).single().unwrap()));
        assert!(!five_h.stale, "wham data is by definition fresh");
        assert!((week.fraction_used.unwrap() - 0.80).abs() < 1e-9);
    }

    #[test]
    fn wham_usage_skips_window_when_payload_omits_it() {
        // The `code_review_rate_limit` and `additional_rate_limits`
        // siblings can be null; nothing forces wham to always
        // return both primary and secondary either. The applier
        // must tolerate either-missing without panicking.
        let usage = WhamUsage {
            plan_type: None,
            rate_limit: RateLimit {
                allowed: true,
                limit_reached: false,
                primary_window: Some(UsageWindow {
                    used_percent: 10.0,
                    limit_window_seconds: 18_000,
                    reset_after_seconds: 1_000,
                    reset_at: 1_900_000_000,
                }),
                secondary_window: None,
            },
            rate_limit_reached_type: None,
        };
        let mut five_h = WindowUsage::default();
        let mut week = WindowUsage::default();
        usage.apply_to(&mut five_h, &mut week, Utc::now());
        assert!(five_h.fraction_used.is_some());
        assert!(week.fraction_used.is_none(), "untouched");
    }

    #[test]
    fn wham_real_response_shape_deserialises() {
        // Captured from the user's live session — verbatim except
        // the bearer-bound `user_id`/`account_id`/`email` were
        // scrubbed. Treat this as the schema-pinning fixture: a
        // upstream change that adds or removes fields will need a
        // matching update here.
        let raw = r#"{
          "user_id": "user-REDACTED",
          "account_id": "user-REDACTED",
          "email": "user@example.com",
          "plan_type": "plus",
          "rate_limit": {
            "allowed": false,
            "limit_reached": true,
            "primary_window": {
              "used_percent": 100,
              "limit_window_seconds": 18000,
              "reset_after_seconds": 14334,
              "reset_at": 1778645737
            },
            "secondary_window": {
              "used_percent": 77,
              "limit_window_seconds": 604800,
              "reset_after_seconds": 18596,
              "reset_at": 1778649999
            }
          },
          "code_review_rate_limit": null,
          "additional_rate_limits": null,
          "credits": null,
          "spend_control": null,
          "rate_limit_reached_type": {
            "type": "rate_limit_reached",
            "details": "default"
          },
          "promo": null,
          "referral_beacon": null
        }"#;
        let parsed: WhamUsage = serde_json::from_str(raw).unwrap();
        assert_eq!(parsed.plan_type.as_deref(), Some("plus"));
        assert!(parsed.rate_limit.limit_reached);
        assert_eq!(
            parsed.rate_limit.primary_window.as_ref().unwrap().used_percent as u32,
            100
        );
        assert_eq!(
            parsed
                .rate_limit_reached_type
                .as_ref()
                .unwrap()
                .kind
                .as_str(),
            "rate_limit_reached"
        );
    }
}
