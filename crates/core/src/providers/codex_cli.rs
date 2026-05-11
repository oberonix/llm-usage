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
//! Schema reverse-engineered against Codex CLI ~0.40.x. Confirmed by
//! cross-reading [soulduse/ai-token-monitor]'s `codex.rs` parser.

use crate::config::CodexCliConfig;
use crate::model::{ProviderId, ProviderStatus, UsageSnapshot, WindowKind};
use crate::provider::Provider;
use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
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

    fn collect_events(&self) -> Result<Vec<TokenEvent>> {
        let mut events = Vec::new();
        if !self.codex_dir.exists() {
            return Ok(events);
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
                if let Err(e) = parse_session_file(entry.path(), &mut events) {
                    tracing::warn!(path = %entry.path().display(), error = %e, "codex parse failed");
                }
            }
        }
        Ok(events)
    }
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

        let events = self.collect_events()?;
        if events.is_empty() {
            return Ok(UsageSnapshot::unavailable(
                ProviderId::CodexCli,
                "no codex sessions found",
            ));
        }

        let five_hour_cutoff = now - Duration::hours(5);
        let week_cutoff = now - Duration::days(7);

        let mut bucket_5h = Bucket::default();
        let mut bucket_7d = Bucket::default();
        for e in &events {
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

        // Codex CLI plan limits aren't exposed in any local file we know of,
        // so we can't compute fraction_used vs a true plan quota. We surface
        // the activity counts and let the user eyeball them; a future config
        // option could supply a manual quota for the alert engine to use.
        snap.headline = Some(if self.cfg.show_spend {
            format!(
                "{} turns / 5h · {} / 7d · ${:.2}",
                bucket_5h.turns, bucket_7d.turns, bucket_7d.cost_usd
            )
        } else {
            format!(
                "{} turns / 5h · {} / 7d",
                bucket_5h.turns, bucket_7d.turns
            )
        });
        if !self.cfg.show_spend {
            snap.strip_spend();
        }
        Ok(snap)
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

fn parse_session_file(path: &std::path::Path, events: &mut Vec<TokenEvent>) -> Result<()> {
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
                let Some(info) = payload.get("info") else {
                    continue;
                };
                if info.is_null() {
                    continue;
                }
                let usage = info.get("last_token_usage").or_else(|| info.get("total_token_usage"));
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

                let timestamp = entry
                    .timestamp
                    .as_deref()
                    .and_then(|s| DateTime::parse_from_rfc3339(s).ok().map(|d| d.with_timezone(&Utc)));
                let Some(ts) = timestamp else { continue };

                events.push(TokenEvent {
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
        let events = p.collect_events().unwrap();
        assert_eq!(events.len(), 2, "should dedupe consecutive identical snapshots");
        assert_eq!(events[0].input_tokens, 1000);
        assert_eq!(events[0].cached_tokens, 100);
        assert_eq!(events[1].input_tokens, 50);
    }
}
