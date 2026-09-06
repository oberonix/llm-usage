//! OpenCode Go usage provider.
//!
//! OpenCode Go (<https://opencode.ai/docs/go/>) is a paid subscription service
//! that provides a curated selection of open-source coding models for a flat
//! monthly fee.
//!
//! ## Data Sources
//!
//! 1. **Live Quota API**:
//!    `GET https://opencode.ai/zen/go/v1/usage`
//!    Authenticated via `Authorization: Bearer <key>`.
//!    Returns usage percentages and reset times across `rolling` (5h),
//!    `weekly` (7d), and `monthly` windows:
//!    ```json
//!    {
//!      "usage": {
//!        "rolling": { "status": "ok", "percent": 42.5, "resetsAt": "2026-09-03T21:05:00Z" },
//!        "weekly": { "status": "ok", "percent": 68.0, "resetsAt": "2026-09-10T00:00:00Z" },
//!        "monthly": { "status": "ok", "percent": 80.0, "resetsAt": "2026-10-01T00:00:00Z" }
//!      }
//!    }
//!    ```
//!
//! 2. **Credential Auto-Discovery**:
//!    The provider automatically reads the API key from
//!    `~/.local/share/opencode/auth.json` (under `"opencode-go"` or `"opencode"`).
//!    Users can also supply an explicit key via `config.toml` (`[opencode_go].api_key`).
//!
//! 3. **Local Token & Turn Activity**:
//!    When `opencode.db` is present, assistant turns with `providerID == "opencode-go"`
//!    are read and merged into `1h`, `today`, `5h`, `week`, and `month` windows,
//!    providing exact token counts and turns without clobbering live quota fractions.

use crate::config::OpenCodeGoConfig;
use crate::model::{ProviderId, ProviderStatus, UsageSnapshot, WindowKind, WindowUsage};
use crate::opencode::{default_db_path, find_opencode_go_api_key, read_events, OpencodeEvent};
use crate::provider::Provider;
use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;

const DEFAULT_BASE_URL: &str = "https://opencode.ai";
const USAGE_PATH: &str = "/zen/go/v1/usage";
const MIN_HTTP_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60);

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct GoUsageResponse {
    #[serde(default)]
    pub usage: GoUsageWindows,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct GoUsageWindows {
    #[serde(default, alias = "five_hour", alias = "5h")]
    pub rolling: Option<GoWindowQuota>,
    #[serde(default, alias = "week", alias = "7d")]
    pub weekly: Option<GoWindowQuota>,
    #[serde(default, alias = "month", alias = "30d")]
    pub monthly: Option<GoWindowQuota>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct GoWindowQuota {
    #[serde(default)]
    pub status: Option<String>,
    pub percent: f64,
    #[serde(default, rename = "resetsAt", alias = "resets_at", alias = "reset_at")]
    pub resets_at: Option<DateTime<Utc>>,
}

struct CachedFetch {
    at: DateTime<Utc>,
    response: GoUsageResponse,
}

pub struct OpenCodeGoProvider {
    cfg: OpenCodeGoConfig,
    http: reqwest::Client,
    opencode_db: Option<PathBuf>,
    base_url: String,
    last_fetch: Arc<Mutex<Option<CachedFetch>>>,
}

impl OpenCodeGoProvider {
    pub fn new(cfg: OpenCodeGoConfig) -> Self {
        Self::with_opencode_db(cfg, Some(default_db_path()))
    }

    pub fn with_opencode_db(cfg: OpenCodeGoConfig, opencode_db: Option<PathBuf>) -> Self {
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .unwrap_or_default();
        let base_url = cfg
            .base_url
            .clone()
            .unwrap_or_else(|| DEFAULT_BASE_URL.to_string());
        let opencode_db = opencode_db.filter(|p| !p.as_os_str().is_empty());
        Self {
            cfg,
            http,
            opencode_db,
            base_url,
            last_fetch: Arc::new(Mutex::new(None)),
        }
    }

    pub fn with_base_url(mut self, base_url: String) -> Self {
        self.base_url = base_url;
        self
    }

    /// Resolve the API key from explicit config or `~/.local/share/opencode/auth.json`.
    pub fn resolve_api_key(&self) -> Option<String> {
        if let Some(ref k) = self.cfg.api_key {
            let trimmed = k.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
        find_opencode_go_api_key(self.cfg.auth_json_path.as_deref())
    }

    /// Fetch usage from the upstream Go endpoint, with in-memory 60s caching.
    async fn fetch_usage_api(&self, key: &str) -> Result<GoUsageResponse> {
        let mut guard = self.last_fetch.lock().await;
        let now = Utc::now();
        if let Some(ref cached) = *guard {
            let elapsed = (now - cached.at).num_seconds();
            if elapsed >= 0 && elapsed < MIN_HTTP_INTERVAL.as_secs() as i64 {
                return Ok(cached.response.clone());
            }
        }

        let url = format!("{}{}", self.base_url.trim_end_matches('/'), USAGE_PATH);
        let resp = self
            .http
            .get(&url)
            .header(reqwest::header::AUTHORIZATION, format!("Bearer {key}"))
            .header(reqwest::header::ACCEPT, "application/json")
            .header(reqwest::header::USER_AGENT, "llm-usage/0.1.1")
            .send()
            .await
            .with_context(|| format!("network error connecting to {url}"))?;

        let status = resp.status();
        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            return Err(anyhow!(
                "auth rejected (HTTP {status}) — invalid or expired OpenCode API key"
            ));
        }
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            return Err(anyhow!(
                "rate limit exceeded (HTTP 429) on OpenCode Go usage API"
            ));
        }
        if !status.is_success() {
            return Err(anyhow!(
                "OpenCode Go usage endpoint returned HTTP {status}"
            ));
        }

        let body = resp
            .text()
            .await
            .context("failed to read OpenCode Go API response body")?;

        let parsed: GoUsageResponse = serde_json::from_str(&body)
            .with_context(|| format!("malformed JSON response from {url}"))?;

        *guard = Some(CachedFetch {
            at: now,
            response: parsed.clone(),
        });

        Ok(parsed)
    }

    fn collect_opencode_events(&self) -> Vec<OpencodeEvent> {
        let Some(db_path) = self.opencode_db.as_ref() else {
            return Vec::new();
        };
        // Try opencode-go first
        let events = read_events(db_path, "opencode-go").unwrap_or_default();
        if !events.is_empty() {
            return events;
        }
        // Fall back to general opencode
        read_events(db_path, "opencode").unwrap_or_default()
    }
}

#[async_trait]
impl Provider for OpenCodeGoProvider {
    fn id(&self) -> ProviderId {
        ProviderId::OpenCodeGo
    }

    fn enabled(&self) -> bool {
        self.cfg.enabled
    }

    async fn poll(&self) -> Result<UsageSnapshot> {
        let now = Utc::now();
        let maybe_key = self.resolve_api_key();
        let local_events = self.collect_opencode_events();

        let (api_result, status, error) = match maybe_key {
            Some(ref key) => match self.fetch_usage_api(key).await {
                Ok(resp) => (Some(resp), ProviderStatus::Ok, None),
                Err(err) => {
                    let err_str = err.to_string();
                    let st = if !local_events.is_empty() {
                        ProviderStatus::Degraded
                    } else {
                        ProviderStatus::Unavailable
                    };
                    (None, st, Some(err_str))
                }
            },
            None => (
                None,
                ProviderStatus::Unavailable,
                Some(
                    "no API key found — run `opencode auth login` or set api_key in config.toml"
                        .to_string(),
                ),
            ),
        };

        let mut windows: BTreeMap<String, WindowUsage> = BTreeMap::new();

        // 1. Populate quota windows from API if available
        if let Some(ref resp) = api_result {
            if let Some(ref r) = resp.usage.rolling {
                let frac = normalize_percent(r.percent);
                windows.insert(
                    WindowKind::FiveHourRolling.label().to_string(),
                    WindowUsage {
                        fraction_used: Some(frac),
                        ends_at: r.resets_at,
                        subview: false,
                        ..Default::default()
                    },
                );
            }
            if let Some(ref w) = resp.usage.weekly {
                let frac = normalize_percent(w.percent);
                windows.insert(
                    WindowKind::ThisWeek.label().to_string(),
                    WindowUsage {
                        fraction_used: Some(frac),
                        ends_at: w.resets_at,
                        subview: false,
                        ..Default::default()
                    },
                );
            }
            if let Some(ref m) = resp.usage.monthly {
                let frac = normalize_percent(m.percent);
                windows.insert(
                    WindowKind::ThisMonth.label().to_string(),
                    WindowUsage {
                        fraction_used: Some(frac),
                        ends_at: m.resets_at,
                        subview: false,
                        ..Default::default()
                    },
                );
            }
        }

        // 2. Fold local token activity from opencode.db
        fold_local_events(&mut windows, &local_events, now);

        // 3. Build headline
        let headline = if maybe_key.is_none() && windows.is_empty() {
            Some("not signed in".to_string())
        } else {
            build_headline(&windows, now)
        };

        Ok(UsageSnapshot {
            provider: ProviderId::OpenCodeGo,
            timestamp: now,
            status,
            error,
            windows,
            headline,
            plan_label: Some("Go".into()),
        })
    }
}

/// Convert percentage into a 0.0..=1.0 fraction.
/// If percent > 1.0 (e.g. 42.5), divides by 100.0.
fn normalize_percent(val: f64) -> f64 {
    let raw = if val > 1.0 { val / 100.0 } else { val };
    raw.clamp(0.0, 1.0)
}

fn fold_local_events(
    windows: &mut BTreeMap<String, WindowUsage>,
    events: &[OpencodeEvent],
    now: DateTime<Utc>,
) {
    if events.is_empty() {
        return;
    }

    let one_hour_ago = now - Duration::hours(1);
    let today_start = now
        .date_naive()
        .and_hms_opt(0, 0, 0)
        .map(|t| DateTime::<Utc>::from_naive_utc_and_offset(t, Utc))
        .unwrap_or(now - Duration::hours(24));
    let five_hours_ago = now - Duration::hours(5);
    let seven_days_ago = now - Duration::days(7);
    let thirty_days_ago = now - Duration::days(30);

    let mut h_in = 0u64;
    let mut h_out = 0u64;
    let mut h_req = 0u64;

    let mut t_in = 0u64;
    let mut t_out = 0u64;
    let mut t_req = 0u64;

    let mut r5_in = 0u64;
    let mut r5_out = 0u64;
    let mut r5_req = 0u64;

    let mut w_in = 0u64;
    let mut w_out = 0u64;
    let mut w_req = 0u64;

    let mut m_in = 0u64;
    let mut m_out = 0u64;
    let mut m_req = 0u64;

    for e in events {
        let ts = e.timestamp;
        if ts >= one_hour_ago {
            h_in += e.input_tokens;
            h_out += e.output_tokens;
            h_req += 1;
        }
        if ts >= today_start {
            t_in += e.input_tokens;
            t_out += e.output_tokens;
            t_req += 1;
        }
        if ts >= five_hours_ago {
            r5_in += e.input_tokens;
            r5_out += e.output_tokens;
            r5_req += 1;
        }
        if ts >= seven_days_ago {
            w_in += e.input_tokens;
            w_out += e.output_tokens;
            w_req += 1;
        }
        if ts >= thirty_days_ago {
            m_in += e.input_tokens;
            m_out += e.output_tokens;
            m_req += 1;
        }
    }

    // Insert 1h and today activity windows if events exist
    if h_req > 0 {
        windows.insert(
            WindowKind::LastHour.label().to_string(),
            WindowUsage {
                tokens_in: h_in,
                tokens_out: h_out,
                request_count: h_req,
                subview: true,
                ..Default::default()
            },
        );
    }
    if t_req > 0 {
        windows.insert(
            WindowKind::Today.label().to_string(),
            WindowUsage {
                tokens_in: t_in,
                tokens_out: t_out,
                request_count: t_req,
                subview: true,
                ..Default::default()
            },
        );
    }

    // Fold metrics into 5h window
    if let Some(w5) = windows.get_mut(WindowKind::FiveHourRolling.label()) {
        w5.tokens_in = r5_in;
        w5.tokens_out = r5_out;
        w5.request_count = r5_req;
    }

    // Fold metrics into week window
    if let Some(ww) = windows.get_mut(WindowKind::ThisWeek.label()) {
        ww.tokens_in = w_in;
        ww.tokens_out = w_out;
        ww.request_count = w_req;
    }

    // Fold metrics into month window
    if let Some(wm) = windows.get_mut(WindowKind::ThisMonth.label()) {
        wm.tokens_in = m_in;
        wm.tokens_out = m_out;
        wm.request_count = m_req;
    }
}

fn build_headline(windows: &BTreeMap<String, WindowUsage>, now: DateTime<Utc>) -> Option<String> {
    let mut parts = Vec::new();

    if let Some(w5) = windows.get(WindowKind::FiveHourRolling.label()) {
        if let Some(frac) = w5.fraction_used {
            let pct = (frac * 100.0).round() as u64;
            let r_str = format_reset(w5.ends_at, now);
            if r_str.is_empty() {
                parts.push(format!("5h {pct}%"));
            } else {
                parts.push(format!("5h {pct}% {r_str}"));
            }
        }
    }

    if let Some(week) = windows.get(WindowKind::ThisWeek.label()) {
        if let Some(frac) = week.fraction_used {
            let pct = (frac * 100.0).round() as u64;
            let r_str = format_reset(week.ends_at, now);
            if r_str.is_empty() {
                parts.push(format!("7d {pct}%"));
            } else {
                parts.push(format!("7d {pct}% {r_str}"));
            }
        }
    }

    if parts.is_empty() {
        if let Some(t) = windows.get(WindowKind::Today.label()) {
            if t.request_count > 0 {
                return Some(format!("{} turns today", t.request_count));
            }
        }
        None
    } else {
        Some(format!("Go · {}", parts.join(" · ")))
    }
}

fn format_reset(ends_at: Option<DateTime<Utc>>, now: DateTime<Utc>) -> String {
    let Some(ends) = ends_at else {
        return String::new();
    };
    if ends <= now {
        return String::new();
    }
    let dur = ends - now;
    let days = dur.num_days();
    if days >= 1 {
        return format!("R:{days}d");
    }
    let hours = dur.num_hours();
    if hours >= 1 {
        return format!("R:{hours}h");
    }
    let mins = dur.num_minutes();
    format!("R:{}m", mins.max(1))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn provider_against(server: &MockServer, key: Option<&str>) -> OpenCodeGoProvider {
        let mut cfg = OpenCodeGoConfig::default();
        cfg.api_key = key.map(|s| s.to_string());
        OpenCodeGoProvider::with_opencode_db(cfg, None).with_base_url(server.uri())
    }

    #[test]
    fn normalize_percent_scales_properly() {
        assert_eq!(normalize_percent(50.0), 0.5);
        assert_eq!(normalize_percent(100.0), 1.0);
        assert_eq!(normalize_percent(0.0), 0.0);
        assert_eq!(normalize_percent(0.42), 0.42);
        assert_eq!(normalize_percent(150.0), 1.0);
    }

    #[tokio::test]
    async fn poll_happy_path_returns_quota_windows() {
        let server = MockServer::start().await;
        let body = r#"{
            "usage": {
                "rolling": { "status": "ok", "percent": 42.0, "resetsAt": "2026-09-03T21:05:00Z" },
                "weekly": { "status": "ok", "percent": 68.0, "resetsAt": "2026-09-10T00:00:00Z" }
            }
        }"#;

        Mock::given(method("GET"))
            .and(path("/zen/go/v1/usage"))
            .and(header("Authorization", "Bearer sk-test-key"))
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .mount(&server)
            .await;

        let provider = provider_against(&server, Some("sk-test-key"));
        let snap = provider.poll().await.unwrap();

        assert_eq!(snap.provider, ProviderId::OpenCodeGo);
        assert_eq!(snap.status, ProviderStatus::Ok);
        assert_eq!(snap.plan_label.as_deref(), Some("Go"));

        let five = snap.windows.get("5h").expect("5h window present");
        assert_eq!(five.fraction_used, Some(0.42));
        assert!(five.ends_at.is_some());

        let week = snap.windows.get("week").expect("week window present");
        assert_eq!(week.fraction_used, Some(0.68));

        let headline = snap.headline.expect("headline present");
        assert!(headline.starts_with("Go · 5h 42%"), "got: {headline}");
        assert!(headline.contains("7d 68%"), "got: {headline}");
    }

    #[tokio::test]
    async fn poll_401_returns_auth_required() {
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/zen/go/v1/usage"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;

        let provider = provider_against(&server, Some("sk-bad-key"));
        let snap = provider.poll().await.unwrap();

        assert_eq!(snap.status, ProviderStatus::Unavailable);
        assert!(snap.error.unwrap().contains("auth rejected"));
    }

    #[tokio::test]
    async fn poll_429_returns_rate_limited() {
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/zen/go/v1/usage"))
            .respond_with(ResponseTemplate::new(429))
            .mount(&server)
            .await;

        let provider = provider_against(&server, Some("sk-key"));
        let snap = provider.poll().await.unwrap();

        assert_eq!(snap.status, ProviderStatus::Unavailable);
        assert!(snap.error.unwrap().contains("rate limit exceeded"));
    }

    #[tokio::test]
    async fn poll_missing_key_returns_informative_unavailable() {
        let dir = TempDir::new().unwrap();
        let auth_path = dir.path().join("auth.json"); // empty dir, file doesn't exist

        let mut cfg = OpenCodeGoConfig::default();
        cfg.api_key = None;
        cfg.auth_json_path = Some(auth_path);

        let provider = OpenCodeGoProvider::with_opencode_db(cfg, None);
        let snap = provider.poll().await.unwrap();

        assert_eq!(snap.status, ProviderStatus::Unavailable);
        assert!(snap.error.unwrap().contains("no API key found"));
        assert_eq!(snap.headline.as_deref(), Some("not signed in"));
    }

    #[tokio::test]
    async fn second_poll_reuses_cache_without_hitting_network() {
        let server = MockServer::start().await;
        let body = r#"{
            "usage": {
                "rolling": { "status": "ok", "percent": 10.0 }
            }
        }"#;

        Mock::given(method("GET"))
            .and(path("/zen/go/v1/usage"))
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .expect(1)
            .mount(&server)
            .await;

        let provider = provider_against(&server, Some("sk-cache-key"));
        let snap1 = provider.poll().await.unwrap();
        assert_eq!(snap1.status, ProviderStatus::Ok);

        let snap2 = provider.poll().await.unwrap();
        assert_eq!(snap2.status, ProviderStatus::Ok);
    }
}
