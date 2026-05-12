//! Reads ~/.claude/projects/**/*.jsonl, extracts per-message usage from
//! Claude Code session logs, and aggregates by time window.
//!
//! Schema (verified against Claude Code 2.1.x JSONL):
//!   { "type": "assistant", "timestamp": "2026-05-07T16:23:08.922Z",
//!     "message": { "model": "claude-opus-4-7",
//!                  "id": "msg_…",   // de-dupe key
//!                  "usage": { "input_tokens": …, "output_tokens": …,
//!                             "cache_read_input_tokens": …,
//!                             "cache_creation": {
//!                                "ephemeral_5m_input_tokens": …,
//!                                "ephemeral_1h_input_tokens": … } } } }

use crate::anthropic_oauth::{
    self, OAuthBackoff, OAuthCredentials, OAuthError, OAuthUsageResponse, QuotaBucket,
};
use crate::config::AnthropicConfig;
use crate::model::{ProviderId, ProviderStatus, UsageSnapshot, WindowKind, WindowUsage};
use crate::pricing::{anthropic_default, AnthropicTokenUsage, ModelRate};
use crate::provider::Provider;
use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Datelike, TimeZone, Utc};
use serde::Deserialize;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;
use walkdir::WalkDir;

pub struct AnthropicProvider {
    cfg: AnthropicConfig,
    projects_dir: PathBuf,
    http: reqwest::Client,
    /// Persists across polls — survives 429s by serving last-good and
    /// holding off the next request until cooldown ends.
    oauth_backoff: Mutex<OAuthBackoff>,
    /// Optional opencode SQLite path; `None` disables the integration.
    opencode_db: Option<PathBuf>,
}

impl AnthropicProvider {
    pub fn new(cfg: AnthropicConfig) -> Self {
        Self::with_opencode_db(cfg, Some(crate::opencode::default_db_path()))
    }

    pub fn with_opencode_db(cfg: AnthropicConfig, opencode_db: Option<PathBuf>) -> Self {
        let projects_dir = cfg
            .claude_projects_dir
            .clone()
            .unwrap_or_else(default_projects_dir);
        let http = reqwest::Client::builder()
            .user_agent(concat!("llm-usage/", env!("CARGO_PKG_VERSION")))
            .timeout(Duration::from_secs(5))
            .build()
            .expect("reqwest");
        let opencode_db = opencode_db.filter(|p| !p.as_os_str().is_empty());
        Self {
            cfg,
            projects_dir,
            http,
            oauth_backoff: Mutex::new(OAuthBackoff::default()),
            opencode_db,
        }
    }

    fn rate_for(&self, model: &str) -> ModelRate {
        for (key, rate) in &self.cfg.model_rates {
            if model.contains(key.as_str()) {
                return *rate;
            }
        }
        anthropic_default(model)
    }

    fn aggregate(&self, now: DateTime<Utc>) -> Result<Aggregate> {
        let mut agg = Aggregate::default();
        if !self.projects_dir.exists() {
            agg.error = Some(format!(
                "claude projects dir not found: {}",
                self.projects_dir.display()
            ));
            return Ok(agg);
        }

        let mut seen_msg_ids: HashSet<String> = HashSet::new();
        for entry in WalkDir::new(&self.projects_dir)
            .follow_links(false)
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.file_type().is_file()
                    && e.path().extension().is_some_and(|x| x == "jsonl")
            })
        {
            if let Err(err) = self.process_file(entry.path(), now, &mut seen_msg_ids, &mut agg) {
                tracing::warn!(path = %entry.path().display(), error = %err, "failed to parse claude jsonl");
            }
        }

        // Supplementary source: opencode's SQLite store. Users hitting
        // Anthropic via opencode rather than Claude Code won't write to
        // ~/.claude/projects, so their turn activity lives there. We
        // run those tokens through the same `rate_for(model)` table so
        // the resulting spend column is consistent with the JSONL path.
        if let Some(db) = &self.opencode_db {
            match crate::opencode::read_events(db, "anthropic") {
                Ok(events) => {
                    for e in events {
                        let usage = AnthropicTokenUsage {
                            input_tokens: e.input_tokens,
                            output_tokens: e.output_tokens,
                            cache_read_input_tokens: e.cached_tokens,
                            cache_creation_5m_input_tokens: 0,
                            cache_creation_1h_input_tokens: 0,
                        };
                        let cost = match e.cost_usd {
                            Some(c) => c,
                            None => usage.cost_usd(self.rate_for(&e.model)),
                        };
                        agg.add(e.timestamp, now, usage, cost);
                    }
                }
                Err(err) => {
                    tracing::warn!(error = %err, path = %db.display(), "opencode db read failed");
                }
            }
        }
        Ok(agg)
    }

    fn process_file(
        &self,
        path: &Path,
        now: DateTime<Utc>,
        seen: &mut HashSet<String>,
        agg: &mut Aggregate,
    ) -> Result<()> {
        let content = std::fs::read_to_string(path)?;
        for (lineno, line) in content.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            let entry: AssistantEntry = match serde_json::from_str(line) {
                Ok(e) => e,
                Err(_) => continue,
            };
            if entry.entry_type.as_deref() != Some("assistant") {
                continue;
            }
            let Some(message) = entry.message else { continue };
            let Some(usage) = message.usage else { continue };
            let model = message.model.unwrap_or_default();
            let timestamp = match entry.timestamp.and_then(|t| {
                DateTime::parse_from_rfc3339(&t)
                    .ok()
                    .map(|d| d.with_timezone(&Utc))
            }) {
                Some(t) => t,
                None => continue,
            };

            // Anthropic returns the same message.id once per turn, but each tool_use
            // produces its own JSONL line carrying the full usage struct. Skip duplicates
            // so we don't multi-count.
            let dedupe_key = format!("{}:{}", path.display(), lineno);
            // Prefer the request-id-bound message id when present.
            let id_key = message
                .id
                .as_deref()
                .map(|id| {
                    entry
                        .request_id
                        .as_deref()
                        .map(|r| format!("{}|{}", r, id))
                        .unwrap_or_else(|| id.to_string())
                })
                .unwrap_or(dedupe_key);
            if !seen.insert(id_key) {
                continue;
            }

            let tokens = AnthropicTokenUsage {
                input_tokens: usage.input_tokens.unwrap_or(0),
                output_tokens: usage.output_tokens.unwrap_or(0),
                cache_read_input_tokens: usage.cache_read_input_tokens.unwrap_or(0),
                cache_creation_5m_input_tokens: usage
                    .cache_creation
                    .as_ref()
                    .and_then(|c| c.ephemeral_5m_input_tokens)
                    .unwrap_or(0),
                cache_creation_1h_input_tokens: usage
                    .cache_creation
                    .as_ref()
                    .and_then(|c| c.ephemeral_1h_input_tokens)
                    .unwrap_or(0),
            };
            let rate = self.rate_for(&model);
            let cost = tokens.cost_usd(rate);

            agg.add(timestamp, now, tokens, cost);
        }
        Ok(())
    }
}

#[async_trait]
impl Provider for AnthropicProvider {
    fn id(&self) -> ProviderId {
        ProviderId::Anthropic
    }
    fn enabled(&self) -> bool {
        self.cfg.enabled
    }
    async fn poll(&self) -> Result<UsageSnapshot> {
        let now = Utc::now();
        let agg = self.aggregate(now)?;
        let mut snap = UsageSnapshot {
            provider: ProviderId::Anthropic,
            timestamp: now,
            status: if agg.error.is_some() {
                ProviderStatus::Degraded
            } else {
                ProviderStatus::Ok
            },
            error: agg.error.clone(),
            windows: Default::default(),
            headline: None,
            plan_label: None,
        };

        // Fill windows.
        let hour = snap.window_mut(WindowKind::LastHour);
        hour.spend_usd = Some(agg.last_hour.cost);
        hour.tokens_in = agg.last_hour.tokens_in;
        hour.tokens_out = agg.last_hour.tokens_out;
        hour.request_count = agg.last_hour.requests;
        hour.recompute_fraction();

        let today = snap.window_mut(WindowKind::Today);
        today.spend_usd = Some(agg.today.cost);
        today.tokens_in = agg.today.tokens_in;
        today.tokens_out = agg.today.tokens_out;
        today.request_count = agg.today.requests;
        today.limit_usd = self.cfg.daily_budget_usd;
        today.recompute_fraction();

        let weekly_fraction = {
            let week = snap.window_mut(WindowKind::ThisWeek);
            week.spend_usd = Some(agg.this_week.cost);
            week.tokens_in = agg.this_week.tokens_in;
            week.tokens_out = agg.this_week.tokens_out;
            week.request_count = agg.this_week.requests;
            week.limit_usd = self.cfg.weekly_budget_usd;
            week.recompute_fraction();
            week.fraction_used
        };

        let month = snap.window_mut(WindowKind::ThisMonth);
        month.spend_usd = Some(agg.this_month.cost);
        month.tokens_in = agg.this_month.tokens_in;
        month.tokens_out = agg.this_month.tokens_out;
        month.request_count = agg.this_month.requests;
        month.recompute_fraction();

        // Quota windows from Anthropic's OAuth /usage endpoint — same data
        // Claude Code's `/usage` shows. We fetch it ourselves on each poll
        // using the access token in ~/.claude/.credentials.json. If we got
        // 429'd recently, serve the last-good cached response instead.
        let mut quota_headline = String::new();
        let mut quota_error: Option<String> = None;
        let mut serving_stale = false;

        let in_cooldown = self
            .oauth_backoff
            .lock()
            .ok()
            .map(|b| b.in_cooldown(now))
            .unwrap_or(false);

        if in_cooldown {
            // Skip the network call entirely; reuse last-good if we have it.
            let snapshot_data = self
                .oauth_backoff
                .lock()
                .ok()
                .and_then(|b| b.last_good.clone());
            if let Some(usage) = snapshot_data {
                apply_oauth_usage(&mut snap, &usage, now, &mut quota_headline);
                serving_stale = true;
            }
            let remaining = self
                .oauth_backoff
                .lock()
                .ok()
                .map(|b| b.cooldown_remaining(now))
                .unwrap_or(0);
            quota_error = Some(format!(
                "rate-limited; quota refresh paused for {}m",
                remaining.div_euclid(60)
            ));
        } else {
            match anthropic_oauth::fetch_usage(&self.http).await {
                Ok(usage) => {
                    apply_oauth_usage(&mut snap, &usage, now, &mut quota_headline);
                    if let Ok(mut b) = self.oauth_backoff.lock() {
                        b.record_success(now, &usage);
                    }
                }
                Err(OAuthError::RateLimited) => {
                    if let Ok(mut b) = self.oauth_backoff.lock() {
                        b.record_429(now);
                        // Serve last-good if any while we're in cooldown.
                        if let Some(usage) = b.last_good.clone() {
                            apply_oauth_usage(&mut snap, &usage, now, &mut quota_headline);
                            serving_stale = true;
                        }
                    }
                    tracing::warn!(
                        "anthropic oauth /usage rate-limited; backing off {}s",
                        OAuthBackoff::INITIAL_COOLDOWN_SECS
                    );
                    quota_error = Some("rate-limited; backing off".into());
                }
                Err(e) => {
                    tracing::warn!(error = %e, "anthropic oauth usage fetch failed");
                    quota_error = Some(e.to_string());
                }
            }
        }

        let stale_marker = if serving_stale { " (stale)" } else { "" };
        snap.headline = if self.cfg.show_spend {
            let mut spend_headline = format!("${:.2} today", agg.today.cost);
            if quota_headline.is_empty() {
                if let Some(f) = weekly_fraction {
                    push_segment(&mut spend_headline, &format!("{:.0}% of weekly", f * 100.0));
                }
            }
            Some(if !quota_headline.is_empty() {
                format!("{}{} · {}", quota_headline, stale_marker, spend_headline)
            } else {
                spend_headline
            })
        } else if !quota_headline.is_empty() {
            Some(format!("{}{}", quota_headline, stale_marker))
        } else {
            None
        };

        // Soften status to Degraded when we have spend data but couldn't
        // reach the quota endpoint — the menu still shows useful info but
        // the user knows quota numbers may be stale.
        if let Some(err) = quota_error {
            if matches!(snap.status, ProviderStatus::Ok) {
                snap.status = ProviderStatus::Degraded;
                snap.error = Some(format!("quota: {}", err));
            }
        }

        // Best-effort plan tag from the credentials file ("Max 5x",
        // "Pro", etc.). Failing to read it is non-fatal — the header
        // just falls back to the provider name alone.
        if let Ok(creds) = OAuthCredentials::load() {
            snap.plan_label = creds.plan_label();
        }

        if !self.cfg.show_spend {
            snap.strip_spend();
        }

        Ok(snap)
    }
}

fn apply_oauth_usage(
    snap: &mut UsageSnapshot,
    usage: &OAuthUsageResponse,
    now: DateTime<Utc>,
    headline: &mut String,
) {
    if let Some(b) = &usage.five_hour {
        fill_quota_window(snap.window_mut(WindowKind::FiveHourRolling), b, now);
        push_segment(headline, &format_quota("5h", b, now));
    }
    if let Some(b) = &usage.seven_day {
        fill_quota_window(snap.window_mut(WindowKind::ThisWeek), b, now);
        push_segment(headline, &format_quota("7d", b, now));
    }
    // Sonnet- and Opus-specific weekly buckets are surfaced as separate
    // labelled windows so the dashboard can show all of them, but we leave
    // them out of the compact tray headline (it's already busy). The
    // display labels match the all-models "week" window so users can read
    // them as siblings of it.
    if let Some(b) = &usage.seven_day_sonnet {
        let w = snap.windows.entry("week (Sonnet)".to_string()).or_default();
        fill_quota_window(w, b, now);
    }
    if let Some(b) = &usage.seven_day_opus {
        let w = snap.windows.entry("week (Opus)".to_string()).or_default();
        fill_quota_window(w, b, now);
    }
}

fn fill_quota_window(window: &mut WindowUsage, q: &QuotaBucket, now: DateTime<Utc>) {
    window.fraction_used = Some(q.utilization / 100.0);
    window.ends_at = q.resets_at_utc();
    window.started_at = Some(now);
}

fn format_quota(label: &str, q: &QuotaBucket, now: DateTime<Utc>) -> String {
    let reset = q
        .resets_at_utc()
        .map(|t| {
            let secs = (t - now).num_seconds();
            if secs <= 0 {
                String::new()
            } else if secs < 3600 {
                format!(" R:{}m", secs / 60)
            } else if secs < 86_400 {
                format!(" R:{}h", secs / 3600)
            } else {
                format!(" R:{}d", secs / 86_400)
            }
        })
        .unwrap_or_default();
    format!("{} {:.0}%{}", label, q.utilization, reset)
}

fn push_segment(s: &mut String, seg: &str) {
    if !s.is_empty() {
        s.push_str(" · ");
    }
    s.push_str(seg);
}

fn default_projects_dir() -> PathBuf {
    dirs::home_dir()
        .map(|h| h.join(".claude").join("projects"))
        .unwrap_or_else(|| PathBuf::from(".claude/projects"))
}

#[derive(Debug, Default, Clone, Copy)]
struct WindowAgg {
    cost: f64,
    tokens_in: u64,
    tokens_out: u64,
    requests: u64,
}

impl WindowAgg {
    fn add(&mut self, t: AnthropicTokenUsage, cost: f64) {
        self.cost += cost;
        self.tokens_in = self.tokens_in.saturating_add(
            t.input_tokens
                + t.cache_read_input_tokens
                + t.cache_creation_5m_input_tokens
                + t.cache_creation_1h_input_tokens,
        );
        self.tokens_out = self.tokens_out.saturating_add(t.output_tokens);
        self.requests += 1;
    }
}

#[derive(Debug, Default)]
struct Aggregate {
    last_hour: WindowAgg,
    today: WindowAgg,
    this_week: WindowAgg,
    this_month: WindowAgg,
    error: Option<String>,
}

impl Aggregate {
    fn add(&mut self, ts: DateTime<Utc>, now: DateTime<Utc>, t: AnthropicTokenUsage, cost: f64) {
        if (now - ts).num_hours().abs() < 1 {
            self.last_hour.add(t, cost);
        }
        if same_local_day(ts, now) {
            self.today.add(t, cost);
        }
        if same_iso_week(ts, now) {
            self.this_week.add(t, cost);
        }
        if same_month(ts, now) {
            self.this_month.add(t, cost);
        }
    }
}

fn same_local_day(a: DateTime<Utc>, b: DateTime<Utc>) -> bool {
    let al = a.with_timezone(&chrono::Local);
    let bl = b.with_timezone(&chrono::Local);
    al.date_naive() == bl.date_naive()
}

fn same_iso_week(a: DateTime<Utc>, b: DateTime<Utc>) -> bool {
    let ai = a.iso_week();
    let bi = b.iso_week();
    ai.year() == bi.year() && ai.week() == bi.week()
}

fn same_month(a: DateTime<Utc>, b: DateTime<Utc>) -> bool {
    a.year() == b.year() && a.month() == b.month()
}

#[allow(dead_code)]
fn epoch_utc() -> DateTime<Utc> {
    Utc.timestamp_opt(0, 0).unwrap()
}

#[derive(Debug, Deserialize)]
struct AssistantEntry {
    #[serde(rename = "type")]
    entry_type: Option<String>,
    timestamp: Option<String>,
    #[serde(rename = "requestId")]
    request_id: Option<String>,
    message: Option<MessageBody>,
}

#[derive(Debug, Deserialize)]
struct MessageBody {
    id: Option<String>,
    model: Option<String>,
    usage: Option<UsageBlock>,
}

#[derive(Debug, Deserialize)]
struct UsageBlock {
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    cache_read_input_tokens: Option<u64>,
    cache_creation: Option<CacheCreation>,
}

#[derive(Debug, Deserialize)]
struct CacheCreation {
    ephemeral_5m_input_tokens: Option<u64>,
    ephemeral_1h_input_tokens: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::TempDir;

    #[test]
    fn push_segment_separates_with_middle_dot() {
        let mut s = String::new();
        push_segment(&mut s, "5h 12%");
        push_segment(&mut s, "7d 30%");
        assert_eq!(s, "5h 12% · 7d 30%");
    }

    #[test]
    fn push_segment_no_leading_separator_on_empty_base() {
        let mut s = String::new();
        push_segment(&mut s, "only");
        assert_eq!(s, "only");
    }

    #[test]
    fn format_quota_shows_reset_in_appropriate_unit() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-05-10T10:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        // 30 minutes ahead → "Rm".
        let b = QuotaBucket {
            utilization: 12.0,
            resets_at: Some("2026-05-10T10:30:00Z".into()),
        };
        let s = format_quota("5h", &b, now);
        assert!(s.contains("12%"));
        assert!(s.contains("R:30m"), "got {}", s);

        // 5 hours ahead → "Rh".
        let b = QuotaBucket {
            utilization: 30.0,
            resets_at: Some("2026-05-10T15:00:00Z".into()),
        };
        let s = format_quota("7d", &b, now);
        assert!(s.contains("30%"));
        assert!(s.contains("R:5h"), "got {}", s);

        // 3 days ahead → "Rd".
        let b = QuotaBucket {
            utilization: 80.0,
            resets_at: Some("2026-05-13T10:00:00Z".into()),
        };
        let s = format_quota("7d", &b, now);
        assert!(s.contains("R:3d"), "got {}", s);
    }

    #[test]
    fn format_quota_omits_reset_when_in_the_past() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-05-10T10:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let b = QuotaBucket {
            utilization: 42.0,
            resets_at: Some("2026-05-10T09:00:00Z".into()),
        };
        let s = format_quota("5h", &b, now);
        assert!(s.contains("42%"));
        assert!(!s.contains("R:"), "got {}", s);
    }

    #[test]
    fn same_local_day_returns_true_for_identical_instants() {
        // Local-day equality is timezone-dependent at the edges; we
        // assert the identity case (same instant) which always holds
        // and the day-apart case which always differs.
        let a = chrono::DateTime::parse_from_rfc3339("2026-05-10T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        assert!(same_local_day(a, a));
        let c = a + chrono::Duration::days(2);
        assert!(!same_local_day(a, c));
    }

    #[test]
    fn same_iso_week_groups_mid_week_timestamps() {
        // Wednesday and Friday of the same ISO week.
        let a = chrono::DateTime::parse_from_rfc3339("2026-05-13T10:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let b = chrono::DateTime::parse_from_rfc3339("2026-05-15T10:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        assert!(same_iso_week(a, b));
    }

    #[test]
    fn same_month_is_year_and_month_strict() {
        let a = chrono::DateTime::parse_from_rfc3339("2026-05-01T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let b = chrono::DateTime::parse_from_rfc3339("2026-05-31T23:59:00Z")
            .unwrap()
            .with_timezone(&Utc);
        assert!(same_month(a, b));
        let c = chrono::DateTime::parse_from_rfc3339("2026-06-01T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        assert!(!same_month(a, c));
        // Different year, same month number → still different.
        let d = chrono::DateTime::parse_from_rfc3339("2025-05-01T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        assert!(!same_month(a, d));
    }

    #[test]
    fn window_agg_add_accumulates_components() {
        let mut a = WindowAgg::default();
        a.add(
            AnthropicTokenUsage {
                input_tokens: 100,
                output_tokens: 50,
                cache_read_input_tokens: 25,
                cache_creation_5m_input_tokens: 10,
                cache_creation_1h_input_tokens: 5,
            },
            0.42,
        );
        assert_eq!(a.requests, 1);
        assert!((a.cost - 0.42).abs() < 1e-9);
        // tokens_in is the sum of input + every cache bucket.
        assert_eq!(a.tokens_in, 100 + 25 + 10 + 5);
        assert_eq!(a.tokens_out, 50);
    }

    #[test]
    fn parses_real_schema() {
        let dir = TempDir::new().unwrap();
        let proj = dir.path().join("project-a");
        std::fs::create_dir_all(&proj).unwrap();
        let f = proj.join("session.jsonl");
        let mut out = std::fs::File::create(&f).unwrap();
        // Two assistant messages in the same request — must dedupe to 1.
        let line = r#"{"type":"assistant","timestamp":"2026-05-08T10:00:00.000Z","requestId":"req_1","message":{"model":"claude-opus-4-7","id":"msg_1","usage":{"input_tokens":100,"output_tokens":200,"cache_read_input_tokens":1000,"cache_creation":{"ephemeral_5m_input_tokens":0,"ephemeral_1h_input_tokens":500}}}}"#;
        writeln!(out, "{}", line).unwrap();
        writeln!(out, "{}", line).unwrap(); // duplicate
        // Second turn, different request, same id — also distinct.
        let line2 = r#"{"type":"assistant","timestamp":"2026-05-08T10:01:00.000Z","requestId":"req_2","message":{"model":"claude-opus-4-7","id":"msg_2","usage":{"input_tokens":50,"output_tokens":50,"cache_read_input_tokens":0,"cache_creation":{"ephemeral_5m_input_tokens":0,"ephemeral_1h_input_tokens":0}}}}"#;
        writeln!(out, "{}", line2).unwrap();
        drop(out);

        let mut cfg = AnthropicConfig::default();
        cfg.claude_projects_dir = Some(dir.path().to_path_buf());
        // Disable opencode merge so we only assert against the JSONL.
        let p = AnthropicProvider::with_opencode_db(cfg, None);
        let now = chrono::Utc.with_ymd_and_hms(2026, 5, 8, 10, 30, 0).unwrap();
        let agg = p.aggregate(now).unwrap();
        assert_eq!(agg.last_hour.requests, 2);
        // 100 input + 200 output + 1000 cache_read + 500 cache_creation_1h, opus rate
        // first turn cost: (100/1M)*15 + (200/1M)*75 + (1000/1M)*15*0.1 + (500/1M)*15*2 = 0.0015+0.015+0.0015+0.015 = 0.033
        // second turn: (50/1M)*15 + (50/1M)*75 = 0.00075 + 0.00375 = 0.0045
        // total ≈ 0.0375
        assert!((agg.today.cost - 0.0375).abs() < 1e-6, "got {}", agg.today.cost);
    }
}
