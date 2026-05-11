//! Codex CLI on ChatGPT plan — no public API. We read the rollout JSONLs
//! under `~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl` (and the optional
//! `~/.codex/archived_sessions/` mirror) to count tokens, requests and a
//! rough spend estimate within rolling 5-hour and 7-day windows.
//!
//! ## Observed schema (per JSONL line):
//!
//! ```text
//! {"type":"session_meta",  "timestamp":"…", "payload":{"id":"<uuid>", …}}
//! {"type":"turn_context",  "timestamp":"…", "payload":{"model":"gpt-5-codex", …}}
//! {"type":"event_msg",     "timestamp":"…", "payload":{
//!     "type":"token_count",
//!     "info":{
//!       "last_token_usage":  {"input_tokens":N,"output_tokens":N,
//!                             "cached_input_tokens":N,"total_tokens":N},
//!       "total_token_usage": { … cumulative … }
//!     }
//! }}
//! ```
//!
//! `last_token_usage` is the per-turn delta; `total_token_usage` is the
//! cumulative-since-session-start rollup. We prefer the delta. Note:
//! OpenAI's `input_tokens` field already includes `cached_input_tokens`
//! as a subset, so for billing we use `(input - cached)` at the regular
//! rate plus `cached` at the cached rate.
//!
//! ## Quota / rate-limit snapshot
//!
//! Same `event_msg{token_count}` payload also carries the live rate-limit
//! state straight from OpenAI's API response:
//!
//! ```json
//! {"payload": {"type":"token_count", "info": null,
//!   "rate_limits": {
//!     "limit_id":"codex","limit_name":null,
//!     "primary":   {"used_percent":1.0,  "window_minutes":300,   "resets_at":1778387659},
//!     "secondary": {"used_percent":17.0, "window_minutes":10080, "resets_at":1778649999},
//!     "credits":null, "plan_type":"plus", "rate_limit_reached_type":null
//!   }}}
//! ```
//!
//! `primary.window_minutes == 300` is the 5-hour rolling window;
//! `secondary.window_minutes == 10080` is the 7-day one. We take the
//! single newest snapshot across all rollouts as authoritative and feed
//! `used_percent` into each window's `fraction_used`. If `resets_at` is
//! already in the past relative to *now*, the window has rolled over
//! since that snapshot was written and we clamp fraction to 0 — the
//! user must run Codex again for a fresh number.
//!
//! Schema reverse-engineered against Codex CLI ~0.40.x. Confirmed by
//! cross-reading [soulduse/ai-token-monitor]'s `codex.rs` parser.

use crate::config::CodexCliConfig;
use crate::model::{ProviderId, ProviderStatus, UsageSnapshot, WindowKind};
use crate::provider::Provider;
use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Duration, TimeZone, Utc};
use serde::Deserialize;
use serde_json::Value;
use std::path::PathBuf;
use walkdir::WalkDir;

pub struct CodexCliProvider {
    cfg: CodexCliConfig,
    codex_dir: PathBuf,
}

impl CodexCliProvider {
    pub fn new(cfg: CodexCliConfig) -> Self {
        let codex_dir = cfg.codex_dir.clone().unwrap_or_else(default_codex_dir);
        Self { cfg, codex_dir }
    }

    fn collect_events(&self) -> Result<Collected> {
        let mut out = Collected::default();
        if !self.codex_dir.exists() {
            return Ok(out);
        }
        // Only the active sessions dir matters for rolling windows up to 7d;
        // we walk archived_sessions too in case ours is configured oddly.
        for sub in ["sessions", "archived_sessions"] {
            let root = self.codex_dir.join(sub);
            if !root.exists() {
                continue;
            }
            for entry in WalkDir::new(&root)
                .into_iter()
                .filter_map(|e| e.ok())
                .filter(|e| e.file_type().is_file() && e.path().extension().is_some_and(|x| x == "jsonl"))
            {
                if let Err(e) = parse_session_file(entry.path(), &mut out) {
                    tracing::warn!(path = %entry.path().display(), error = %e, "codex parse failed");
                }
            }
        }
        Ok(out)
    }
}

#[derive(Debug, Default)]
struct Collected {
    events: Vec<TokenEvent>,
    /// Newest `rate_limits` snapshot seen across every rollout, keyed on
    /// the timestamp of the carrying `event_msg`. Used to set
    /// fraction_used + ends_at on the two windows.
    latest_rate_limits: Option<RateLimitsRecord>,
}

#[derive(Debug, Clone)]
struct RateLimitsRecord {
    /// When the rollout line that carried this snapshot was written.
    /// Used both to pick the newest record and to decide whether
    /// `resets_at` has lapsed since.
    record_at: DateTime<Utc>,
    primary: Option<RateLimitsBucket>,
    secondary: Option<RateLimitsBucket>,
    plan_type: Option<String>,
    rate_limit_reached_type: Option<String>,
}

#[derive(Debug, Clone, Copy)]
struct RateLimitsBucket {
    used_percent: f64,
    window_minutes: u32,
    resets_at: Option<DateTime<Utc>>,
}

#[async_trait]
impl Provider for CodexCliProvider {
    fn id(&self) -> ProviderId {
        ProviderId::CodexCli
    }
    fn enabled(&self) -> bool {
        self.cfg.enabled
    }
    async fn poll(&self) -> Result<UsageSnapshot> {
        let now = Utc::now();
        if !self.codex_dir.exists() {
            return Ok(UsageSnapshot::unavailable(
                ProviderId::CodexCli,
                format!("no codex dir at {}", self.codex_dir.display()),
            ));
        }

        let collected = self.collect_events()?;
        if collected.events.is_empty() && collected.latest_rate_limits.is_none() {
            return Ok(UsageSnapshot::unavailable(
                ProviderId::CodexCli,
                "no codex sessions found",
            ));
        }

        let five_hour_cutoff = now - Duration::hours(5);
        let week_cutoff = now - Duration::days(7);

        let mut bucket_5h = Bucket::default();
        let mut bucket_7d = Bucket::default();
        for e in &collected.events {
            if e.timestamp > five_hour_cutoff {
                bucket_5h.add(e);
            }
            if e.timestamp > week_cutoff {
                bucket_7d.add(e);
            }
        }

        let mut snap = UsageSnapshot {
            provider: ProviderId::CodexCli,
            timestamp: now,
            status: ProviderStatus::Ok,
            error: None,
            windows: Default::default(),
            headline: None,
        };

        let w5 = snap.window_mut(WindowKind::FiveHourRolling);
        w5.tokens_in = bucket_5h.input_tokens;
        w5.tokens_out = bucket_5h.output_tokens;
        w5.request_count = bucket_5h.turns;
        w5.spend_usd = Some(bucket_5h.cost_usd);

        let ww = snap.window_mut(WindowKind::ThisWeek);
        ww.tokens_in = bucket_7d.input_tokens;
        ww.tokens_out = bucket_7d.output_tokens;
        ww.request_count = bucket_7d.turns;
        ww.spend_usd = Some(bucket_7d.cost_usd);

        // Apply the most recent rate-limit snapshot we observed. Codex
        // emits these on every API turn; if the snapshot's window has
        // already rolled over (resets_at < now), we clamp fraction to
        // 0 — the user hasn't run Codex since the window reset, so
        // there's no fresher number to fold in.
        let mut plan_label: Option<String> = None;
        if let Some(rl) = &collected.latest_rate_limits {
            plan_label = rl.plan_type.clone();
            if let Some(primary) = rl.primary {
                apply_rate_limits(
                    snap.window_mut(WindowKind::FiveHourRolling),
                    primary,
                    now,
                );
            }
            if let Some(secondary) = rl.secondary {
                apply_rate_limits(snap.window_mut(WindowKind::ThisWeek), secondary, now);
            }
            if rl.rate_limit_reached_type.is_some() {
                snap.status = ProviderStatus::Degraded;
                snap.error = Some(format!(
                    "rate limit hit: {}",
                    rl.rate_limit_reached_type.as_deref().unwrap_or("?")
                ));
            }
        }

        snap.headline = Some(build_headline(
            plan_label.as_deref(),
            snap.window(WindowKind::FiveHourRolling),
            snap.window(WindowKind::ThisWeek),
            bucket_5h.turns,
            bucket_7d.turns,
            self.cfg.show_spend.then_some(bucket_7d.cost_usd),
        ));
        if !self.cfg.show_spend {
            snap.strip_spend();
        }
        Ok(snap)
    }
}

fn apply_rate_limits(
    w: &mut crate::model::WindowUsage,
    bucket: RateLimitsBucket,
    now: DateTime<Utc>,
) {
    let raw_frac = bucket.used_percent / 100.0;
    // If the snapshot's window has already rolled, the percentage is
    // stale — clamp to 0 rather than showing a phantom utilization.
    let still_in_window = bucket.resets_at.is_some_and(|t| t > now);
    w.fraction_used = Some(if still_in_window { raw_frac } else { 0.0 });
    w.ends_at = bucket.resets_at;
    w.started_at = Some(now);
    let _ = bucket.window_minutes; // kept for future labelling
}

fn build_headline(
    plan: Option<&str>,
    w5: Option<&crate::model::WindowUsage>,
    w7: Option<&crate::model::WindowUsage>,
    turns_5h: u64,
    turns_7d: u64,
    spend_7d: Option<f64>,
) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(p) = plan {
        parts.push(p.to_string());
    }
    if let Some(w) = w5 {
        if let Some(frac) = w.fraction_used {
            parts.push(format!(
                "5h {:.0}%{}",
                frac * 100.0,
                reset_suffix(w.ends_at)
            ));
        } else {
            parts.push(format!("{} turns / 5h", turns_5h));
        }
    }
    if let Some(w) = w7 {
        if let Some(frac) = w.fraction_used {
            parts.push(format!(
                "7d {:.0}%{}",
                frac * 100.0,
                reset_suffix(w.ends_at)
            ));
        } else {
            parts.push(format!("{} turns / 7d", turns_7d));
        }
    }
    if let Some(spend) = spend_7d {
        parts.push(format!("${:.2}", spend));
    }
    parts.join(" · ")
}

fn reset_suffix(ends_at: Option<DateTime<Utc>>) -> String {
    let Some(t) = ends_at else { return String::new() };
    let now = Utc::now();
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
}

fn default_codex_dir() -> PathBuf {
    dirs::home_dir()
        .map(|h| h.join(".codex"))
        .unwrap_or_else(|| PathBuf::from(".codex"))
}

#[derive(Debug, Default)]
struct Bucket {
    turns: u64,
    input_tokens: u64,
    output_tokens: u64,
    cached_tokens: u64,
    cost_usd: f64,
}

impl Bucket {
    fn add(&mut self, e: &TokenEvent) {
        if e.input_tokens == 0 && e.output_tokens == 0 && e.cached_tokens == 0 {
            return;
        }
        self.turns += 1;
        self.input_tokens = self.input_tokens.saturating_add(e.input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(e.output_tokens);
        self.cached_tokens = self.cached_tokens.saturating_add(e.cached_tokens);
        self.cost_usd += codex_cost_usd(&e.model, e.input_tokens, e.output_tokens, e.cached_tokens);
    }
}

#[derive(Debug, Clone)]
struct TokenEvent {
    timestamp: DateTime<Utc>,
    model: String,
    input_tokens: u64,
    output_tokens: u64,
    cached_tokens: u64,
}

#[derive(Debug, Deserialize)]
struct OuterEntry {
    #[serde(rename = "type")]
    entry_type: Option<String>,
    timestamp: Option<String>,
    payload: Option<Value>,
}

fn parse_session_file(path: &std::path::Path, out: &mut Collected) -> Result<()> {
    let content = std::fs::read_to_string(path)?;
    let mut current_model = String::new();
    let mut prev_snapshot: Option<(u64, u64, u64)> = None;
    for line in content.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let entry: OuterEntry = match serde_json::from_str(line) {
            Ok(e) => e,
            Err(_) => continue,
        };
        let payload = match entry.payload {
            Some(p) => p,
            None => continue,
        };
        let record_ts = entry
            .timestamp
            .as_deref()
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok().map(|d| d.with_timezone(&Utc)));

        match entry.entry_type.as_deref() {
            Some("turn_context") => {
                if let Some(m) = payload.get("model").and_then(|v| v.as_str()) {
                    current_model = m.to_string();
                }
            }
            Some("event_msg") => {
                if payload.get("type").and_then(|v| v.as_str()) != Some("token_count") {
                    continue;
                }

                // Rate-limit snapshot, if present. Lives on the
                // event_msg payload itself, not under `info`.
                if let Some(rl) = payload.get("rate_limits") {
                    if let (Some(ts), Some(record)) =
                        (record_ts, parse_rate_limits(rl, record_ts))
                    {
                        let take = match &out.latest_rate_limits {
                            None => true,
                            Some(prev) => ts > prev.record_at,
                        };
                        if take {
                            let _ = ts; // satisfy clippy's unused warning when feature gating
                            out.latest_rate_limits = Some(record);
                        }
                    }
                }

                let info = payload.get("info");
                let usage = info
                    .filter(|v| !v.is_null())
                    .and_then(|i| i.get("last_token_usage").or_else(|| i.get("total_token_usage")));
                let Some(u) = usage else { continue };
                let input = u.get("input_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
                let output = u.get("output_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
                let cached = u.get("cached_input_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
                if input == 0 && output == 0 && cached == 0 {
                    continue;
                }
                // Skip duplicate consecutive snapshots — Codex emits cumulative
                // total_token_usage on each event so identical snapshots in a row
                // mean "no progress this turn".
                let snap = (input, output, cached);
                if prev_snapshot == Some(snap) {
                    continue;
                }
                prev_snapshot = Some(snap);

                let Some(ts) = record_ts else { continue };

                out.events.push(TokenEvent {
                    timestamp: ts,
                    model: if current_model.is_empty() {
                        "gpt-5-codex".into()
                    } else {
                        current_model.clone()
                    },
                    input_tokens: input,
                    output_tokens: output,
                    cached_tokens: cached,
                });
            }
            _ => {}
        }
    }
    Ok(())
}

fn parse_rate_limits(v: &Value, record_at: Option<DateTime<Utc>>) -> Option<RateLimitsRecord> {
    let record_at = record_at?;
    let primary = v.get("primary").and_then(parse_rate_limits_bucket);
    let secondary = v.get("secondary").and_then(parse_rate_limits_bucket);
    if primary.is_none() && secondary.is_none() {
        return None;
    }
    Some(RateLimitsRecord {
        record_at,
        primary,
        secondary,
        plan_type: v
            .get("plan_type")
            .and_then(|x| x.as_str())
            .map(|s| s.to_string()),
        rate_limit_reached_type: v
            .get("rate_limit_reached_type")
            .and_then(|x| x.as_str())
            .map(|s| s.to_string()),
    })
}

fn parse_rate_limits_bucket(v: &Value) -> Option<RateLimitsBucket> {
    let used_percent = v.get("used_percent")?.as_f64()?;
    let window_minutes = v.get("window_minutes")?.as_u64()? as u32;
    let resets_at = v
        .get("resets_at")
        .and_then(|x| x.as_i64())
        .and_then(|secs| Utc.timestamp_opt(secs, 0).single());
    Some(RateLimitsBucket {
        used_percent,
        window_minutes,
        resets_at,
    })
}

/// Conservative pricing defaults (USD per 1M tokens). User can override
/// via config in a future iteration. OpenAI's `input_tokens` includes
/// `cached_input_tokens` as a subset, so we charge `(input - cached)` at
/// the regular input rate and `cached` at the cached rate.
fn codex_cost_usd(model: &str, input: u64, output: u64, cached: u64) -> f64 {
    let m = model.to_ascii_lowercase();
    let (i_rate, o_rate, c_rate) = if m.contains("gpt-5") || m.contains("codex") {
        (1.25, 10.0, 0.125)
    } else if m.contains("gpt-4o") {
        (2.5, 10.0, 1.25)
    } else {
        (1.0, 4.0, 0.5)
    };
    let billed_input = input.saturating_sub(cached);
    (billed_input as f64 * i_rate
        + output as f64 * o_rate
        + cached as f64 * c_rate)
        / 1_000_000.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::TempDir;

    #[test]
    fn parses_real_codex_schema() {
        let dir = TempDir::new().unwrap();
        let day = dir.path().join("sessions/2026/05/08");
        std::fs::create_dir_all(&day).unwrap();
        let f = day.join("rollout-test.jsonl");
        let mut out = std::fs::File::create(&f).unwrap();
        writeln!(out, r#"{{"type":"session_meta","timestamp":"2026-05-08T10:00:00Z","payload":{{"id":"sess-1"}}}}"#).unwrap();
        writeln!(out, r#"{{"type":"turn_context","timestamp":"2026-05-08T10:00:01Z","payload":{{"model":"gpt-5-codex"}}}}"#).unwrap();
        writeln!(out, r#"{{"type":"event_msg","timestamp":"2026-05-08T10:00:05Z","payload":{{"type":"token_count","info":{{"last_token_usage":{{"input_tokens":1000,"output_tokens":200,"cached_input_tokens":100,"total_tokens":1200}}}}}}}}"#).unwrap();
        // Duplicate snapshot — should be skipped.
        writeln!(out, r#"{{"type":"event_msg","timestamp":"2026-05-08T10:00:06Z","payload":{{"type":"token_count","info":{{"last_token_usage":{{"input_tokens":1000,"output_tokens":200,"cached_input_tokens":100,"total_tokens":1200}}}}}}}}"#).unwrap();
        // Different snapshot — counted.
        writeln!(out, r#"{{"type":"event_msg","timestamp":"2026-05-08T10:00:09Z","payload":{{"type":"token_count","info":{{"last_token_usage":{{"input_tokens":50,"output_tokens":50,"cached_input_tokens":0,"total_tokens":100}}}}}}}}"#).unwrap();
        drop(out);

        let mut cfg = CodexCliConfig::default();
        cfg.codex_dir = Some(dir.path().to_path_buf());
        let p = CodexCliProvider::new(cfg);
        let collected = p.collect_events().unwrap();
        assert_eq!(
            collected.events.len(),
            2,
            "should dedupe consecutive identical snapshots"
        );
        assert_eq!(collected.events[0].input_tokens, 1000);
        assert_eq!(collected.events[0].cached_tokens, 100);
        assert_eq!(collected.events[1].input_tokens, 50);
    }

    #[test]
    fn parses_real_rate_limits_schema() {
        let dir = TempDir::new().unwrap();
        let day = dir.path().join("sessions/2026/05/09");
        std::fs::create_dir_all(&day).unwrap();
        let f = day.join("rollout-rl.jsonl");
        let mut out = std::fs::File::create(&f).unwrap();
        // Real shape from a live ollama.com user's rollout: rate_limits
        // sits on the event_msg payload alongside `info`, NOT inside it.
        writeln!(
            out,
            r#"{{"timestamp":"2026-05-09T23:34:19.205Z","type":"event_msg","payload":{{"type":"token_count","info":null,"rate_limits":{{"limit_id":"codex","limit_name":null,"primary":{{"used_percent":1.0,"window_minutes":300,"resets_at":1778387659}},"secondary":{{"used_percent":17.0,"window_minutes":10080,"resets_at":1778649999}},"credits":null,"plan_type":"plus","rate_limit_reached_type":null}}}}}}"#
        )
        .unwrap();
        // Older rollout with a *smaller* used_percent — must NOT override
        // the newer record above.
        writeln!(
            out,
            r#"{{"timestamp":"2026-05-09T20:00:00.000Z","type":"event_msg","payload":{{"type":"token_count","info":null,"rate_limits":{{"limit_id":"codex","primary":{{"used_percent":50.0,"window_minutes":300,"resets_at":1778000000}},"plan_type":"plus"}}}}}}"#
        )
        .unwrap();
        drop(out);

        let mut cfg = CodexCliConfig::default();
        cfg.codex_dir = Some(dir.path().to_path_buf());
        let p = CodexCliProvider::new(cfg);
        let collected = p.collect_events().unwrap();
        let rl = collected
            .latest_rate_limits
            .expect("rate_limits should be captured");
        assert_eq!(rl.plan_type.as_deref(), Some("plus"));
        // Newest record (23:34Z) wins, not the older 50% one.
        let primary = rl.primary.expect("primary present");
        assert!((primary.used_percent - 1.0).abs() < 1e-6);
        assert_eq!(primary.window_minutes, 300);
        assert_eq!(
            primary.resets_at,
            Utc.timestamp_opt(1778387659, 0).single()
        );
        let secondary = rl.secondary.expect("secondary present");
        assert!((secondary.used_percent - 17.0).abs() < 1e-6);
        assert_eq!(secondary.window_minutes, 10080);
    }
}
