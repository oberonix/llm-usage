//! Ollama Cloud usage scraper.
//!
//! Ollama publishes no usage API, so we scrape the logged-in
//! `/settings` page using a browser session cookie the user pastes
//! into config (or captures via `llm-usage-setup` / `rookie`). Auth: the
//! cookie is sent as the raw `Cookie:` header, exactly as the browser would.
//!
//! ## Page shape (verified against ollama.com 2026-05-09)
//!
//! The settings page is server-rendered (no Next.js initial-data blob),
//! and the entire usage panel is a `<h2>Cloud Usage</h2>` followed by
//! two `<div>` blocks of the form:
//!
//! ```html
//! <h2><span>Cloud Usage</span><span class="… capitalize">pro</span></h2>
//! <div>
//!   <div class="flex justify-between mb-2">
//!     <span class="text-sm">Session usage</span>
//!     <span class="text-sm">27.8% used</span>
//!   </div>
//!   <div … style="width: 27.8%"></div>
//!   <div class="local-time" data-time="2026-05-10T03:00:00Z">Resets in 2 hours</div>
//! </div>
//! <div>
//!   <div class="flex justify-between mb-2">
//!     <span class="text-sm">Weekly usage</span>
//!     <span class="text-sm">83.5% used</span>
//!   </div>
//!   …data-time="2026-05-11T00:00:00Z"…
//! </div>
//! ```
//!
//! There are no token counts and no dollar amounts — Ollama's settings
//! page is purely percentage-based. That suits us: the rest of the app
//! defaults to quota-only display anyway.

use crate::config::OllamaCloudConfig;
use crate::model::{ProviderId, ProviderStatus, UsageSnapshot, WindowKind, WindowUsage};
use crate::provider::Provider;
use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use chrono::{DateTime, Datelike, Utc};
use regex::Regex;
use scraper::{ElementRef, Html, Selector};
use std::time::Duration;

const BASE_URL: &str = "https://ollama.com";
const SETTINGS_PATH: &str = "/settings";

/// Minimum time between `/settings` HTTP calls. Within this window
/// we reuse the cached HTML so file-watcher-driven refreshes don't
/// hammer ollama.com (which has no documented rate limit but is
/// well within bot-detection territory if we knock too fast).
const MIN_HTTP_INTERVAL: Duration = Duration::from_secs(60);

struct CachedFetch {
    at: chrono::DateTime<Utc>,
    html: String,
}

pub struct OllamaCloudProvider {
    cfg: OllamaCloudConfig,
    http: reqwest::Client,
    /// Optional opencode SQLite path; `None` disables the integration.
    opencode_db: Option<std::path::PathBuf>,
    /// Base URL for the settings request. Production default is
    /// `BASE_URL`; tests inject a wiremock URI via [`with_base_url`].
    base_url: String,
    /// Last successful `/settings` fetch, served on cache-hit. Only
    /// successful responses are stored — auth failures and parse
    /// failures must keep re-trying.
    last_good: std::sync::Mutex<Option<CachedFetch>>,
}

impl OllamaCloudProvider {
    pub fn new(cfg: OllamaCloudConfig) -> Self {
        Self::with_opencode_db(cfg, Some(crate::opencode::default_db_path()))
    }

    pub fn with_opencode_db(
        cfg: OllamaCloudConfig,
        opencode_db: Option<std::path::PathBuf>,
    ) -> Self {
        let http = reqwest::Client::builder()
            // Browser-like UA so the page doesn't decide we're a bot and
            // serve a stripped HTML shell.
            .user_agent(
                "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 \
                 (KHTML, like Gecko) Chrome/130.0 Safari/537.36",
            )
            .timeout(Duration::from_secs(15))
            .build()
            .expect("reqwest");
        let opencode_db = opencode_db.filter(|p| !p.as_os_str().is_empty());
        Self {
            cfg,
            http,
            opencode_db,
            base_url: BASE_URL.to_string(),
            last_good: std::sync::Mutex::new(None),
        }
    }

    /// Replace the base URL used for the settings request. Only tests
    /// should need this — production always talks to ollama.com.
    pub fn with_base_url(mut self, base: impl Into<String>) -> Self {
        self.base_url = base.into();
        self
    }

    /// Public so the `dump_ollama_cloud` example can reuse the auth path.
    pub async fn fetch_settings_html(&self) -> Result<String> {
        let cookie = self
            .cfg
            .session_cookie
            .as_deref()
            .ok_or_else(|| anyhow!("no session_cookie set"))?;
        let url = format!("{}{}", self.base_url.trim_end_matches('/'), SETTINGS_PATH);
        let resp = self
            .http
            .get(&url)
            .header(reqwest::header::COOKIE, cookie)
            .header(reqwest::header::ACCEPT, "text/html,application/xhtml+xml")
            .send()
            .await
            .with_context(|| format!("GET {}", url))?;
        if resp.status() == reqwest::StatusCode::UNAUTHORIZED
            || resp.status() == reqwest::StatusCode::FORBIDDEN
        {
            return Err(anyhow!(
                "auth rejected ({}) — session cookie likely expired",
                resp.status()
            ));
        }
        if !resp.status().is_success() {
            return Err(anyhow!("settings page {}", resp.status()));
        }
        let text = resp.text().await.context("read body")?;
        Ok(text)
    }
}

#[async_trait]
impl Provider for OllamaCloudProvider {
    fn id(&self) -> ProviderId {
        ProviderId::OllamaCloud
    }
    fn enabled(&self) -> bool {
        self.cfg.enabled
    }
    async fn poll(&self) -> Result<UsageSnapshot> {
        if self.cfg.session_cookie.is_none() {
            return Ok(UsageSnapshot::unavailable(
                ProviderId::OllamaCloud,
                "Not signed in — use the dashboard's Ollama Cloud setup",
            ));
        }

        // Cache-hit short-circuit: if we successfully fetched within
        // the throttle window, skip the HTTP and reuse the prior body.
        let now_pre = Utc::now();
        let cached_html: Option<String> = self.last_good.lock().ok().and_then(|guard| {
            guard.as_ref().and_then(|c| {
                let elapsed = (now_pre - c.at).to_std().ok()?;
                if elapsed < MIN_HTTP_INTERVAL {
                    Some(c.html.clone())
                } else {
                    None
                }
            })
        });

        let html = if let Some(cached) = cached_html {
            cached
        } else {
            match self.fetch_settings_html().await {
                Ok(h) => {
                    if let Ok(mut guard) = self.last_good.lock() {
                        *guard = Some(CachedFetch {
                            at: Utc::now(),
                            html: h.clone(),
                        });
                    }
                    h
                }
                Err(e) => {
                    return Ok(UsageSnapshot::unavailable(
                        ProviderId::OllamaCloud,
                        format!("Fetch failed: {}", e),
                    ));
                }
            }
        };

        let parsed = parse_settings(&html);
        if parsed.rows.is_empty() && parsed.plan.is_none() {
            return Ok(UsageSnapshot::unavailable(
                ProviderId::OllamaCloud,
                "Parse failed — page shape changed; run `cargo run -p llm-usage-core \
                 --example dump_ollama_cloud` and tighten selectors",
            ));
        }

        let now = Utc::now();
        let mut snap = UsageSnapshot {
            provider: ProviderId::OllamaCloud,
            timestamp: now,
            status: ProviderStatus::Ok,
            error: None,
            windows: Default::default(),
            headline: None,
            plan_label: parsed.plan.as_deref().map(crate::model::title_case_first),
        };

        for row in &parsed.rows {
            // Normalise the short rolling window to "5h" so it aligns
            // with the same-named row from Anthropic and Codex even
            // though Ollama calls it "Session usage" on the page.
            let key = match row.label.as_str() {
                "Session usage" => "5h".to_string(),
                "Weekly usage" => WindowKind::ThisWeek.label().to_string(),
                other => other.to_ascii_lowercase().replace(' ', "-"),
            };
            let w: &mut WindowUsage = snap.windows.entry(key).or_default();
            w.fraction_used = Some(row.percent / 100.0);
            w.ends_at = row.reset_at;
            w.started_at = Some(now);
            w.mark_stale_if_expired(now);
        }

        // Supplementary source: opencode token activity scoped to
        // ollama-cloud. The page scrape doesn't surface tokens at all,
        // so this is purely additive context — it fills in the 1h /
        // today / week / month rows on the dashboard without touching
        // the fraction_used we already populated above.
        if let Some(db) = &self.opencode_db {
            match crate::opencode::read_events(db, "ollama-cloud") {
                Ok(events) => fold_opencode_events(&mut snap, &events, now),
                Err(err) => {
                    tracing::warn!(error = %err, path = %db.display(), "opencode db read failed");
                }
            }
        }

        snap.headline = Some(build_headline(&parsed, now));
        Ok(snap)
    }
}

/// Bucket opencode events into 1h / today / 5h / week / month windows
/// and accumulate tokens, requests, and cost. Doesn't touch
/// `fraction_used` — those come from the page scrape.
fn fold_opencode_events(
    snap: &mut UsageSnapshot,
    events: &[crate::opencode::OpencodeEvent],
    now: DateTime<Utc>,
) {
    let hour_cutoff = now - chrono::Duration::hours(1);
    let five_hour_cutoff = now - chrono::Duration::hours(5);
    let week_cutoff = now - chrono::Duration::days(7);
    let today = now.date_naive();
    let this_month = (now.year(), now.month());

    for e in events {
        // Build the list of windows this event qualifies for.
        let mut buckets: Vec<&'static str> = Vec::new();
        if e.timestamp > hour_cutoff {
            buckets.push("1h");
        }
        if e.timestamp.date_naive() == today {
            buckets.push("today");
        }
        if e.timestamp > five_hour_cutoff {
            buckets.push("5h");
        }
        if e.timestamp > week_cutoff {
            buckets.push(WindowKind::ThisWeek.label());
        }
        if (e.timestamp.year(), e.timestamp.month()) == this_month {
            buckets.push("month");
        }
        for label in &buckets {
            let w = snap.windows.entry((*label).to_string()).or_default();
            w.tokens_in = w.tokens_in.saturating_add(e.input_tokens + e.cached_tokens);
            w.tokens_out = w.tokens_out.saturating_add(e.output_tokens);
            w.request_count = w.request_count.saturating_add(1);
            if let Some(cost) = e.cost_usd {
                w.spend_usd = Some(w.spend_usd.unwrap_or(0.0) + cost);
            }
        }
    }
}

#[derive(Debug, Default)]
struct Parsed {
    plan: Option<String>,
    rows: Vec<UsageRow>,
}

#[derive(Debug)]
struct UsageRow {
    label: String,
    percent: f64,
    reset_at: Option<DateTime<Utc>>,
}

/// Walk the DOM looking for the two `<div class="flex justify-between …">`
/// header rows that pair a label span with an "X% used" span. The
/// reset timestamp lives in a sibling `<div class="local-time" data-time="…">`
/// under the same usage block — we find it by jumping up to the row's
/// grandparent and scanning its descendants.
fn parse_settings(html: &str) -> Parsed {
    let doc = Html::parse_document(html);
    let mut out = Parsed::default();

    // Plan: <h2 …>Cloud Usage<span class="capitalize">pro</span></h2>
    if let Ok(plan_sel) = Selector::parse("h2 span.capitalize") {
        if let Some(el) = doc.select(&plan_sel).next() {
            let txt: String = el.text().collect::<String>().trim().to_string();
            if !txt.is_empty() {
                out.plan = Some(txt);
            }
        }
    }

    let row_sel = match Selector::parse("div.flex.justify-between") {
        Ok(s) => s,
        Err(_) => return out,
    };
    let span_sel = Selector::parse("span.text-sm").unwrap();
    let local_time_sel = Selector::parse("div.local-time").unwrap();
    // "27.8% used" — allow integers and decimals, ignore surrounding whitespace.
    let percent_re = Regex::new(r"^\s*([0-9]+(?:\.[0-9]+)?)\s*%\s*used\s*$").unwrap();

    for header in doc.select(&row_sel) {
        let spans: Vec<ElementRef> = header.select(&span_sel).collect();
        if spans.len() < 2 {
            continue;
        }
        let label: String = spans[0].text().collect::<String>().trim().to_string();
        let percent_text: String = spans[1].text().collect::<String>().trim().to_string();
        let Some(cap) = percent_re.captures(&percent_text) else {
            continue;
        };
        let Ok(percent) = cap[1].parse::<f64>() else {
            continue;
        };

        // Reset time lives on the row's grandparent: header is the
        // `flex justify-between` div, parent is the row container,
        // and the local-time div is a sibling of header inside parent.
        let mut reset_at: Option<DateTime<Utc>> = None;
        if let Some(parent_node) = header.parent() {
            if let Some(parent_el) = ElementRef::wrap(parent_node) {
                if let Some(lt) = parent_el.select(&local_time_sel).next() {
                    if let Some(t) = lt.value().attr("data-time") {
                        reset_at = DateTime::parse_from_rfc3339(t)
                            .ok()
                            .map(|d| d.with_timezone(&Utc));
                    }
                }
            }
        }

        out.rows.push(UsageRow {
            label,
            percent,
            reset_at,
        });
    }

    out
}

fn build_headline(parsed: &Parsed, now: DateTime<Utc>) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(p) = &parsed.plan {
        parts.push(p.clone());
    }
    for row in &parsed.rows {
        let short_label = match row.label.as_str() {
            "Session usage" => "5h",
            "Weekly usage" => "7d",
            other => other,
        };
        let reset = row
            .reset_at
            .map(|t| format_reset(t, now))
            .unwrap_or_default();
        parts.push(format!("{} {:.0}%{}", short_label, row.percent, reset));
    }
    if parts.is_empty() {
        "scraped".into()
    } else {
        parts.join(" · ")
    }
}

fn format_reset(reset: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let secs = (reset - now).num_seconds();
    if secs <= 0 {
        return String::new();
    }
    if secs < 3600 {
        format!(" R:{}m", secs / 60)
    } else if secs < 86_400 {
        format!(" R:{}h", secs / 3600)
    } else {
        format!(" R:{}d", secs / 86_400)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Trimmed but real-shape sample of the ollama.com /settings page,
    /// captured 2026-05-09. If Ollama redesigns the page this test is
    /// the first place we'll see selectors break.
    const SAMPLE: &str = r#"
<html><body>
  <div>
    <h2 class="text-xl font-medium flex items-center space-x-2">
      <span>Cloud Usage</span>
      <span class="text-xs font-normal px-2 py-0.5 rounded-full bg-neutral-100 text-neutral-600 capitalize">pro</span>
    </h2>
    <div>
      <div class="flex justify-between mb-2">
        <span class="text-sm">Session usage</span>
        <span class="text-sm">27.8% used</span>
      </div>
      <div class="w-full border border-1 border-neutral-200 rounded-full h-2 overflow-hidden">
        <div class="h-full rounded-full bg-neutral-300" style="width: 27.8%"></div>
      </div>
      <div class="text-xs text-neutral-500 mt-1 local-time" data-time="2026-05-10T03:00:00Z">Resets in 2 hours</div>
    </div>
    <div>
      <div class="flex justify-between mb-2">
        <span class="text-sm">Weekly usage</span>
        <span class="text-sm">83.5% used</span>
      </div>
      <div class="w-full border border-1 border-neutral-200 rounded-full h-2 overflow-hidden">
        <div class="h-full rounded-full bg-neutral-300" style="width: 83.5%"></div>
      </div>
      <div class="text-xs text-neutral-500 mt-1 local-time" data-time="2026-05-11T00:00:00Z">Resets in 23 hours</div>
    </div>
  </div>
</body></html>
"#;

    #[test]
    fn parses_real_settings_layout() {
        let parsed = parse_settings(SAMPLE);
        assert_eq!(parsed.plan.as_deref(), Some("pro"));
        assert_eq!(parsed.rows.len(), 2);

        assert_eq!(parsed.rows[0].label, "Session usage");
        assert!((parsed.rows[0].percent - 27.8).abs() < 1e-6);
        assert_eq!(
            parsed.rows[0].reset_at,
            DateTime::parse_from_rfc3339("2026-05-10T03:00:00Z")
                .ok()
                .map(|d| d.with_timezone(&Utc))
        );

        assert_eq!(parsed.rows[1].label, "Weekly usage");
        assert!((parsed.rows[1].percent - 83.5).abs() < 1e-6);
    }

    #[test]
    fn missing_panel_yields_empty_parsed() {
        let parsed = parse_settings("<html><body>Hello</body></html>");
        assert!(parsed.rows.is_empty());
        assert!(parsed.plan.is_none());
    }

    #[test]
    fn build_headline_combines_plan_and_rows() {
        let parsed = parse_settings(SAMPLE);
        // Use a fixed `now` so the reset suffix is predictable.
        let now = DateTime::parse_from_rfc3339("2026-05-10T01:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let h = build_headline(&parsed, now);
        // 03:00Z is 2h away → "R:2h"; 2026-05-11T00:00 is 23h away → "R:23h".
        // Short window is normalised to "5h" so it matches the label
        // every other provider uses.
        assert!(h.contains("pro"), "{}", h);
        assert!(h.contains("5h 28% R:2h"), "{}", h);
        assert!(h.contains("7d 84% R:23h"), "{}", h);
    }

    #[test]
    fn format_reset_picks_largest_unit_under_threshold() {
        let now = DateTime::parse_from_rfc3339("2026-05-10T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        // 30 minutes ahead → minutes.
        let r = now + chrono::Duration::minutes(30);
        assert_eq!(format_reset(r, now), " R:30m");
        // 2 hours ahead → hours.
        let r = now + chrono::Duration::hours(2);
        assert_eq!(format_reset(r, now), " R:2h");
        // 3 days ahead → days.
        let r = now + chrono::Duration::days(3);
        assert_eq!(format_reset(r, now), " R:3d");
    }

    #[test]
    fn format_reset_empty_when_in_the_past() {
        let now = DateTime::parse_from_rfc3339("2026-05-10T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let r = now - chrono::Duration::minutes(1);
        assert_eq!(format_reset(r, now), "");
    }

    #[test]
    fn parse_settings_picks_up_plan_badge() {
        // A truncated sample with just the plan; rows can be empty.
        let html = r#"
<html><body><h2><span>Cloud Usage</span><span class="capitalize">free</span></h2></body></html>"#;
        let parsed = parse_settings(html);
        assert_eq!(parsed.plan.as_deref(), Some("free"));
        assert!(parsed.rows.is_empty());
    }

    #[test]
    fn integer_percent_parses() {
        // No decimal — must still match the regex.
        let html = r#"
<html><body><h2><span>Cloud Usage</span></h2>
<div><div class="flex justify-between"><span class="text-sm">Session usage</span><span class="text-sm">100% used</span></div></div>
</body></html>"#;
        let parsed = parse_settings(html);
        assert_eq!(parsed.rows.len(), 1);
        assert!((parsed.rows[0].percent - 100.0).abs() < 1e-6);
    }

    // ---- HTTP-layer tests against wiremock ----
    //
    // These exercise the path from `poll()` through `fetch_settings_html`
    // and back: auth failures, transport failures, body-parse failures,
    // and the happy path. We disable the opencode integration in all of
    // them so the assertions stay focused on what the page-scrape did.

    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn provider_against(server: &MockServer, cookie: &str) -> OllamaCloudProvider {
        let mut cfg = OllamaCloudConfig::default();
        cfg.enabled = true;
        cfg.session_cookie = Some(cookie.into());
        // Disable opencode merge: an empty path string is the documented
        // way to opt out, see `Config::resolve_opencode_db`.
        OllamaCloudProvider::with_opencode_db(cfg, None).with_base_url(server.uri())
    }

    #[tokio::test]
    async fn fetch_settings_returns_body_on_200() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/settings"))
            .and(header("Cookie", "session=abc"))
            .respond_with(ResponseTemplate::new(200).set_body_string(SAMPLE))
            .expect(1)
            .mount(&server)
            .await;
        let p = provider_against(&server, "session=abc");
        let body = p.fetch_settings_html().await.unwrap();
        assert!(body.contains("Session usage"));
    }

    #[tokio::test]
    async fn fetch_settings_401_yields_auth_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/settings"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;
        let p = provider_against(&server, "session=stale");
        let err = p.fetch_settings_html().await.unwrap_err().to_string();
        assert!(err.contains("session cookie"), "got: {err}");
    }

    #[tokio::test]
    async fn fetch_settings_403_yields_auth_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/settings"))
            .respond_with(ResponseTemplate::new(403))
            .mount(&server)
            .await;
        let p = provider_against(&server, "session=stale");
        let err = p.fetch_settings_html().await.unwrap_err().to_string();
        assert!(err.contains("session cookie"), "got: {err}");
    }

    #[tokio::test]
    async fn fetch_settings_500_yields_generic_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/settings"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        let p = provider_against(&server, "session=ok");
        let err = p.fetch_settings_html().await.unwrap_err().to_string();
        assert!(err.contains("500"), "got: {err}");
    }

    #[tokio::test]
    async fn fetch_settings_missing_cookie_errors_before_request() {
        let server = MockServer::start().await;
        // No mock registered → if we attempted the request the test
        // would fail with "unexpected request" or a network error.
        // The session_cookie=None guard rejects before that point.
        let mut cfg = OllamaCloudConfig::default();
        cfg.enabled = true;
        cfg.session_cookie = None;
        let p = OllamaCloudProvider::with_opencode_db(cfg, None).with_base_url(server.uri());
        let err = p.fetch_settings_html().await.unwrap_err().to_string();
        assert!(err.contains("no session_cookie"), "got: {err}");
    }

    #[tokio::test]
    async fn poll_happy_path_populates_quota_windows() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/settings"))
            .respond_with(ResponseTemplate::new(200).set_body_string(SAMPLE))
            .mount(&server)
            .await;
        let p = provider_against(&server, "session=abc");
        let snap = p.poll().await.unwrap();
        assert_eq!(snap.status, ProviderStatus::Ok);
        assert_eq!(snap.plan_label.as_deref(), Some("Pro"));
        // Session usage is normalised to the "5h" label.
        let five_h = snap.windows.get("5h").expect("5h window present");
        assert!((five_h.fraction_used.unwrap() - 0.278).abs() < 1e-6);
        // Weekly usage rolls into the canonical week label.
        let week = snap
            .windows
            .get(WindowKind::ThisWeek.label())
            .expect("week window present");
        assert!((week.fraction_used.unwrap() - 0.835).abs() < 1e-6);
    }

    #[tokio::test]
    async fn poll_returns_unavailable_when_body_has_no_markers() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/settings"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string("<html><body>maintenance</body></html>"),
            )
            .mount(&server)
            .await;
        let p = provider_against(&server, "session=abc");
        let snap = p.poll().await.unwrap();
        assert_eq!(snap.status, ProviderStatus::Unavailable);
        assert!(snap
            .error
            .as_deref()
            .unwrap_or("")
            .to_lowercase()
            .contains("parse failed"));
    }

    #[tokio::test]
    async fn poll_returns_unavailable_when_http_fails() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/settings"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        let p = provider_against(&server, "session=abc");
        let snap = p.poll().await.unwrap();
        // `poll` swallows fetch errors into an unavailable snapshot
        // rather than failing the whole tray loop.
        assert_eq!(snap.status, ProviderStatus::Unavailable);
        assert!(snap
            .error
            .as_deref()
            .unwrap_or("")
            .to_lowercase()
            .contains("fetch failed"));
    }

    #[tokio::test]
    async fn poll_without_cookie_is_unavailable_with_setup_hint() {
        let server = MockServer::start().await;
        let mut cfg = OllamaCloudConfig::default();
        cfg.enabled = true;
        cfg.session_cookie = None;
        let p = OllamaCloudProvider::with_opencode_db(cfg, None).with_base_url(server.uri());
        let snap = p.poll().await.unwrap();
        assert_eq!(snap.status, ProviderStatus::Unavailable);
        assert!(snap
            .error
            .as_deref()
            .unwrap_or("")
            .to_lowercase()
            .contains("not signed in"));
    }

    #[tokio::test]
    async fn second_poll_within_throttle_window_reuses_cache_no_http() {
        // Two back-to-back polls. The mock is configured to expect
        // *exactly one* HTTP call (`.expect(1)`); a second `/settings`
        // request would fail the test on server drop. Confirms that
        // the in-process cache short-circuits the network entirely.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/settings"))
            .respond_with(ResponseTemplate::new(200).set_body_string(SAMPLE))
            .expect(1)
            .mount(&server)
            .await;
        let p = provider_against(&server, "session=abc");
        let first = p.poll().await.unwrap();
        assert_eq!(first.status, ProviderStatus::Ok);
        let second = p.poll().await.unwrap();
        // Same fractions, same plan label — the cache replays the
        // identical HTML so the parsed result is bit-for-bit equal.
        assert_eq!(second.status, ProviderStatus::Ok);
        assert_eq!(second.plan_label, first.plan_label);
        assert_eq!(
            second.windows.get("5h").unwrap().fraction_used,
            first.windows.get("5h").unwrap().fraction_used,
        );
    }
}
