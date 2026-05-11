//! Gemini CLI usage tracker. Walks `~/.gemini/tmp/<project>/chats/*.jsonl`
//! and aggregates `type=gemini` turns.
//!
//! ## Schema (verified against gemini-cli 0.41.x):
//!
//! Each session file is a JSONL stream. The first line is session metadata:
//!
//! ```json
//! {"sessionId":"…","projectHash":"…","startTime":"…","kind":"main"}
//! ```
//!
//! Conversation turns alternate user/gemini, with `$set` updates between:
//!
//! ```json
//! {"id":"…","timestamp":"…","type":"user","content":[{"text":"…"}]}
//! {"$set":{"lastUpdated":"…"}}
//! {"id":"…","timestamp":"…","type":"gemini","content":"…","thoughts":[],
//!  "tokens":{"input":N,"output":N,"cached":N,"thoughts":N,"tool":N,"total":N},
//!  "model":"gemini-3-flash-preview"}
//! ```
//!
//! We only count `type=gemini` lines. `cached` is a subset of `input` (same
//! convention OpenAI uses), so for spend calc we charge `(input - cached)` at
//! the regular input rate plus `cached` at the cached rate. Output and
//! "thinking" tokens are billed at the output rate (Gemini bills internal
//! thoughts as output on Pro tier).

use crate::config::GeminiCliConfig;
use crate::model::{ProviderId, ProviderStatus, UsageSnapshot, WindowKind};
use crate::provider::Provider;
use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Datelike, Duration, Utc};
use serde::Deserialize;
use std::path::PathBuf;
use walkdir::WalkDir;

pub struct GeminiCliProvider {
    cfg: GeminiCliConfig,
    gemini_dir: PathBuf,
}

impl GeminiCliProvider {
    pub fn new(cfg: GeminiCliConfig) -> Self {
        let gemini_dir = cfg.gemini_dir.clone().unwrap_or_else(default_gemini_dir);
        Self { cfg, gemini_dir }
    }

    fn collect_events(&self) -> Vec<TokenEvent> {
        let mut events = Vec::new();
        let chats_root = self.gemini_dir.join("tmp");
        if !chats_root.exists() {
            return events;
        }
        for entry in WalkDir::new(&chats_root)
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.file_type().is_file()
                    && e.path().extension().is_some_and(|x| x == "jsonl")
                    && e.path()
                        .components()
                        .any(|c| c.as_os_str() == "chats")
            })
        {
            let Ok(content) = std::fs::read_to_string(entry.path()) else {
                continue;
            };
            for line in content.lines() {
                if line.trim().is_empty() {
                    continue;
                }
                let Ok(record) = serde_json::from_str::<TurnLine>(line) else {
                    continue;
                };
                if record.entry_type.as_deref() != Some("gemini") {
                    continue;
                }
                let Some(tokens) = record.tokens else { continue };
                let Some(ts_str) = record.timestamp else { continue };
                let Ok(ts_fixed) = DateTime::parse_from_rfc3339(&ts_str) else {
                    continue;
                };
                events.push(TokenEvent {
                    timestamp: ts_fixed.with_timezone(&Utc),
                    model: record.model.unwrap_or_else(|| "gemini".into()),
                    input_tokens: tokens.input.unwrap_or(0),
                    output_tokens: tokens.output.unwrap_or(0),
                    cached_tokens: tokens.cached.unwrap_or(0),
                    thoughts_tokens: tokens.thoughts.unwrap_or(0),
                });
            }
        }
        events
    }
}

fn default_gemini_dir() -> PathBuf {
    dirs::home_dir()
        .map(|h| h.join(".gemini"))
        .unwrap_or_else(|| PathBuf::from(".gemini"))
}

#[async_trait]
impl Provider for GeminiCliProvider {
    fn id(&self) -> ProviderId {
        ProviderId::GeminiCli
    }
    fn enabled(&self) -> bool {
        self.cfg.enabled
    }
    async fn poll(&self) -> Result<UsageSnapshot> {
        if !self.gemini_dir.exists() {
            return Ok(UsageSnapshot::unavailable(
                ProviderId::GeminiCli,
                format!("no gemini dir at {}", self.gemini_dir.display()),
            ));
        }
        let now = Utc::now();
        let events = self.collect_events();
        if events.is_empty() {
            return Ok(UsageSnapshot::unavailable(
                ProviderId::GeminiCli,
                "no gemini sessions found",
            ));
        }

        let hour_cutoff = now - Duration::hours(1);
        let week_cutoff = now - Duration::days(7);
        let mut hour = Bucket::default();
        let mut today = Bucket::default();
        let mut week = Bucket::default();
        let mut month = Bucket::default();
        for e in &events {
            if e.timestamp > hour_cutoff {
                hour.add(e);
            }
            if same_local_day(e.timestamp, now) {
                today.add(e);
            }
            if e.timestamp > week_cutoff {
                week.add(e);
            }
            if same_month(e.timestamp, now) {
                month.add(e);
            }
        }

        let mut snap = UsageSnapshot {
            provider: ProviderId::GeminiCli,
            timestamp: now,
            status: ProviderStatus::Ok,
            error: None,
            windows: Default::default(),
            headline: None,
        };

        let h = snap.window_mut(WindowKind::LastHour);
        h.spend_usd = Some(hour.cost_usd);
        h.tokens_in = hour.input_tokens;
        h.tokens_out = hour.output_tokens;
        h.request_count = hour.turns;

        let t = snap.window_mut(WindowKind::Today);
        t.spend_usd = Some(today.cost_usd);
        t.tokens_in = today.input_tokens;
        t.tokens_out = today.output_tokens;
        t.request_count = today.turns;

        let w = snap.window_mut(WindowKind::ThisWeek);
        w.spend_usd = Some(week.cost_usd);
        w.tokens_in = week.input_tokens;
        w.tokens_out = week.output_tokens;
        w.request_count = week.turns;

        let m = snap.window_mut(WindowKind::ThisMonth);
        m.spend_usd = Some(month.cost_usd);
        m.tokens_in = month.input_tokens;
        m.tokens_out = month.output_tokens;
        m.request_count = month.turns;
        m.limit_usd = self.cfg.monthly_budget_usd;
        m.recompute_fraction();

        snap.headline = Some(if self.cfg.show_spend {
            format!(
                "${:.2} today · ${:.2} 7d · {} turns",
                today.cost_usd, week.cost_usd, week.turns
            )
        } else {
            format!(
                "{} turns today · {} turns 7d",
                today.turns, week.turns
            )
        });
        if !self.cfg.show_spend {
            snap.strip_spend();
        }
        Ok(snap)
    }
}

#[derive(Debug, Default)]
struct Bucket {
    turns: u64,
    input_tokens: u64,
    output_tokens: u64,
    cached_tokens: u64,
    thoughts_tokens: u64,
    cost_usd: f64,
}

impl Bucket {
    fn add(&mut self, e: &TokenEvent) {
        self.turns += 1;
        self.input_tokens = self.input_tokens.saturating_add(e.input_tokens);
        self.output_tokens = self
            .output_tokens
            .saturating_add(e.output_tokens.saturating_add(e.thoughts_tokens));
        self.cached_tokens = self.cached_tokens.saturating_add(e.cached_tokens);
        self.thoughts_tokens = self.thoughts_tokens.saturating_add(e.thoughts_tokens);
        self.cost_usd += gemini_cost_usd(
            &e.model,
            e.input_tokens,
            e.output_tokens.saturating_add(e.thoughts_tokens),
            e.cached_tokens,
        );
    }
}

#[derive(Debug, Clone)]
struct TokenEvent {
    timestamp: DateTime<Utc>,
    model: String,
    input_tokens: u64,
    output_tokens: u64,
    cached_tokens: u64,
    thoughts_tokens: u64,
}

#[derive(Debug, Deserialize)]
struct TurnLine {
    #[serde(rename = "type")]
    entry_type: Option<String>,
    timestamp: Option<String>,
    model: Option<String>,
    tokens: Option<TokenBlock>,
}

#[derive(Debug, Deserialize)]
struct TokenBlock {
    input: Option<u64>,
    output: Option<u64>,
    cached: Option<u64>,
    thoughts: Option<u64>,
    #[allow(dead_code)]
    tool: Option<u64>,
    #[allow(dead_code)]
    total: Option<u64>,
}

/// Conservative per-1M-token defaults. Override per-model in config when
/// Google updates pricing.
fn gemini_cost_usd(model: &str, input: u64, output: u64, cached: u64) -> f64 {
    let m = model.to_ascii_lowercase();
    let (i_rate, o_rate, c_rate) = if m.contains("3-pro") || m.contains("3.0-pro") {
        (1.25, 10.0, 0.25)
    } else if m.contains("3-flash") || m.contains("3.0-flash") {
        (0.30, 2.50, 0.075)
    } else if m.contains("2.5-pro") {
        (1.25, 10.0, 0.31)
    } else if m.contains("2.5-flash") {
        (0.30, 2.50, 0.075)
    } else {
        (0.30, 2.50, 0.075)
    };
    let billed_input = input.saturating_sub(cached);
    (billed_input as f64 * i_rate
        + output as f64 * o_rate
        + cached as f64 * c_rate)
        / 1_000_000.0
}

fn same_local_day(a: DateTime<Utc>, b: DateTime<Utc>) -> bool {
    let al = a.with_timezone(&chrono::Local);
    let bl = b.with_timezone(&chrono::Local);
    al.date_naive() == bl.date_naive()
}

fn same_month(a: DateTime<Utc>, b: DateTime<Utc>) -> bool {
    a.year() == b.year() && a.month() == b.month()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::TempDir;

    #[test]
    fn parses_real_gemini_schema() {
        let dir = TempDir::new().unwrap();
        let chats = dir.path().join("tmp/myproj/chats");
        std::fs::create_dir_all(&chats).unwrap();
        let f = chats.join("session-test.jsonl");
        let mut out = std::fs::File::create(&f).unwrap();
        writeln!(out, r#"{{"sessionId":"s1","kind":"main","startTime":"2026-05-08T10:00:00Z"}}"#).unwrap();
        writeln!(out, r#"{{"id":"u1","timestamp":"2026-05-08T10:00:01Z","type":"user","content":[{{"text":"hi"}}]}}"#).unwrap();
        writeln!(out, r#"{{"$set":{{"lastUpdated":"2026-05-08T10:00:02Z"}}}}"#).unwrap();
        writeln!(out, r#"{{"id":"g1","timestamp":"2026-05-08T10:00:05Z","type":"gemini","model":"gemini-3-flash-preview","tokens":{{"input":12000,"output":50,"cached":8000,"thoughts":10,"tool":0,"total":12060}}}}"#).unwrap();
        drop(out);

        let mut cfg = GeminiCliConfig::default();
        cfg.gemini_dir = Some(dir.path().to_path_buf());
        let p = GeminiCliProvider::new(cfg);
        let events = p.collect_events();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].input_tokens, 12000);
        assert_eq!(events[0].cached_tokens, 8000);
        assert_eq!(events[0].thoughts_tokens, 10);
    }
}
