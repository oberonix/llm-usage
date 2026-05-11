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
use chrono::{DateTime, Utc};
use regex::Regex;
use scraper::{ElementRef, Html, Selector};
use std::time::Duration;

const BASE_URL: &str = "https://ollama.com";
const SETTINGS_PATH: &str = "/settings";

pub struct OllamaCloudProvider {
    cfg: OllamaCloudConfig,
    http: reqwest::Client,
}

impl OllamaCloudProvider {
    pub fn new(cfg: OllamaCloudConfig) -> Self {
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
        Self { cfg, http }
    }

    /// Public so the `dump_ollama_cloud` example can reuse the auth path.
    pub async fn fetch_settings_html(&self) -> Result<String> {
        let cookie = self
            .cfg
            .session_cookie
            .as_deref()
            .ok_or_else(|| anyhow!("no session_cookie set"))?;
        let url = format!("{}{}", BASE_URL, SETTINGS_PATH);
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
                "not signed in — use the dashboard's Ollama Cloud setup",
            ));
        }

        let html = match self.fetch_settings_html().await {
            Ok(h) => h,
            Err(e) => {
                return Ok(UsageSnapshot::unavailable(
                    ProviderId::OllamaCloud,
                    format!("fetch failed: {}", e),
                ));
            }
        };

        let parsed = parse_settings(&html);
        if parsed.rows.is_empty() && parsed.plan.is_none() {
            return Ok(UsageSnapshot::unavailable(
                ProviderId::OllamaCloud,
                "parse failed — page shape changed; run `cargo run -p llm-usage-core \
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
            plan_label: parsed
                .plan
                .as_deref()
                .map(crate::model::title_case_first),
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
        }

        snap.headline = Some(build_headline(&parsed, now));
        Ok(snap)
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
}
