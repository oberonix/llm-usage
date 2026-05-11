//! Day-bucketed historical aggregation for the dashboard charts.
//! Re-walks the Claude Code JSONL tree on demand. Cheap enough for small
//! personal histories; if it ever gets slow, persist to SQLite instead.

use chrono::{DateTime, Duration, NaiveDate, Utc};
use llm_usage_core::config::AnthropicConfig;
use llm_usage_core::pricing::{anthropic_default, AnthropicTokenUsage};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::PathBuf;
use walkdir::WalkDir;

pub fn anthropic_daily_spend(cfg: &AnthropicConfig, days: i64) -> Vec<(NaiveDate, f64)> {
    let projects_dir = cfg
        .claude_projects_dir
        .clone()
        .unwrap_or_else(|| {
            dirs::home_dir()
                .map(|h| h.join(".claude").join("projects"))
                .unwrap_or_else(|| PathBuf::from(".claude/projects"))
        });
    if !projects_dir.exists() {
        return Vec::new();
    }
    let now = Utc::now();
    let cutoff = now - Duration::days(days);

    let mut buckets: BTreeMap<NaiveDate, f64> = BTreeMap::new();
    // pre-seed empty days
    for i in 0..days {
        let d = (now - Duration::days(days - 1 - i))
            .with_timezone(&chrono::Local)
            .date_naive();
        buckets.insert(d, 0.0);
    }

    let mut seen = std::collections::HashSet::new();
    for entry in WalkDir::new(&projects_dir)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.file_type().is_file()
                && e.path().extension().is_some_and(|x| x == "jsonl")
        })
    {
        let Ok(content) = std::fs::read_to_string(entry.path()) else {
            continue;
        };
        for (lineno, line) in content.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            let Ok(record) = serde_json::from_str::<AssistantEntry>(line) else {
                continue;
            };
            if record.entry_type.as_deref() != Some("assistant") {
                continue;
            }
            let Some(message) = record.message else { continue };
            let Some(usage) = message.usage else { continue };
            let Some(ts_str) = record.timestamp else { continue };
            let Ok(ts_fixed) = DateTime::parse_from_rfc3339(&ts_str) else {
                continue;
            };
            let ts: DateTime<Utc> = ts_fixed.with_timezone(&Utc);
            if ts < cutoff {
                continue;
            }
            let dedupe = format!("{}:{}", entry.path().display(), lineno);
            let id_key = message
                .id
                .as_deref()
                .map(|id| {
                    record
                        .request_id
                        .as_deref()
                        .map(|r| format!("{}|{}", r, id))
                        .unwrap_or_else(|| id.to_string())
                })
                .unwrap_or(dedupe);
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
            let model = message.model.unwrap_or_default();
            let mut rate = anthropic_default(&model);
            for (key, override_rate) in &cfg.model_rates {
                if model.contains(key.as_str()) {
                    rate = *override_rate;
                }
            }
            let cost = tokens.cost_usd(rate);
            let local_date = ts.with_timezone(&chrono::Local).date_naive();
            *buckets.entry(local_date).or_insert(0.0) += cost;
        }
    }

    let mut out: Vec<(NaiveDate, f64)> = buckets.into_iter().collect();
    out.sort_by_key(|(d, _)| *d);
    out
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
