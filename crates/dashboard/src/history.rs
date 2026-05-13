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
    anthropic_daily_spend_at(cfg, days, Utc::now())
}

/// Same as [`anthropic_daily_spend`] but with the "now" reference time
/// injectable so tests can write timestamped fixtures without flaking
/// when the clock rolls past midnight.
pub fn anthropic_daily_spend_at(
    cfg: &AnthropicConfig,
    days: i64,
    now: DateTime<Utc>,
) -> Vec<(NaiveDate, f64)> {
    let projects_dir = cfg.claude_projects_dir.clone().unwrap_or_else(|| {
        dirs::home_dir()
            .map(|h| h.join(".claude").join("projects"))
            .unwrap_or_else(|| PathBuf::from(".claude/projects"))
    });
    if !projects_dir.exists() {
        return Vec::new();
    }
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
        .filter(|e| e.file_type().is_file() && e.path().extension().is_some_and(|x| x == "jsonl"))
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
            let Some(message) = record.message else {
                continue;
            };
            let Some(usage) = message.usage else { continue };
            let Some(ts_str) = record.timestamp else {
                continue;
            };
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

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use std::io::Write;
    use tempfile::TempDir;

    /// Builds an `AnthropicConfig` whose projects dir points at the
    /// given path, with empty `model_rates` so default rates apply.
    fn cfg_for(dir: &std::path::Path) -> AnthropicConfig {
        let mut cfg = AnthropicConfig::default();
        cfg.claude_projects_dir = Some(dir.to_path_buf());
        cfg
    }

    /// Emit one well-formed assistant-line JSONL record into `f`.
    fn write_assistant(
        f: &mut std::fs::File,
        ts_rfc3339: &str,
        req_id: &str,
        msg_id: &str,
        model: &str,
        input: u64,
        output: u64,
    ) {
        let line = format!(
            r#"{{"type":"assistant","timestamp":"{ts}","requestId":"{rid}","message":{{"model":"{m}","id":"{mid}","usage":{{"input_tokens":{i},"output_tokens":{o},"cache_read_input_tokens":0,"cache_creation":{{"ephemeral_5m_input_tokens":0,"ephemeral_1h_input_tokens":0}}}}}}}}"#,
            ts = ts_rfc3339,
            rid = req_id,
            mid = msg_id,
            m = model,
            i = input,
            o = output,
        );
        writeln!(f, "{}", line).unwrap();
    }

    #[test]
    fn returns_empty_when_projects_dir_is_absent() {
        // Point at a path that doesn't exist on disk. Must be a
        // sub-path of a real tempdir (not "/__no__") so the test stays
        // hermetic across users with weird permissions on `/`.
        let dir = TempDir::new().unwrap();
        let cfg = cfg_for(&dir.path().join("no-such-subdir"));
        let now = Utc::now();
        assert!(anthropic_daily_spend_at(&cfg, 7, now).is_empty());
    }

    #[test]
    fn empty_projects_dir_still_returns_preseeded_zero_days() {
        // The function seeds every day in the window with 0.0 so the
        // dashboard chart shows a flat baseline. Confirm the count
        // matches the requested window even when there's no data.
        let dir = TempDir::new().unwrap();
        let cfg = cfg_for(dir.path());
        let now = Utc::now();
        let out = anthropic_daily_spend_at(&cfg, 5, now);
        assert_eq!(out.len(), 5);
        assert!(out.iter().all(|(_, v)| *v == 0.0));
    }

    #[test]
    fn aggregates_assistant_entries_by_local_date() {
        let dir = TempDir::new().unwrap();
        let proj = dir.path().join("project-a");
        std::fs::create_dir_all(&proj).unwrap();
        let path = proj.join("session.jsonl");
        let mut f = std::fs::File::create(&path).unwrap();

        let day1 = Utc.with_ymd_and_hms(2026, 5, 6, 12, 0, 0).unwrap();
        let day2 = Utc.with_ymd_and_hms(2026, 5, 7, 12, 0, 0).unwrap();
        write_assistant(
            &mut f,
            &day1.to_rfc3339(),
            "req_1",
            "msg_1",
            "claude-opus-4-7",
            1000,
            200,
        );
        write_assistant(
            &mut f,
            &day2.to_rfc3339(),
            "req_2",
            "msg_2",
            "claude-opus-4-7",
            500,
            100,
        );

        let cfg = cfg_for(dir.path());
        // "now" is 2026-05-09: both records fall within a 7-day window.
        let now = Utc.with_ymd_and_hms(2026, 5, 9, 0, 0, 0).unwrap();
        let out = anthropic_daily_spend_at(&cfg, 7, now);
        let total: f64 = out.iter().map(|(_, v)| *v).sum();
        // Two assistant turns => non-zero spend; we don't assert the
        // exact dollar value here because pricing changes shouldn't
        // break this test. Instead confirm the bucket distribution.
        assert!(total > 0.0, "got {:#?}", out);
        let day1_local = day1.with_timezone(&chrono::Local).date_naive();
        let day2_local = day2.with_timezone(&chrono::Local).date_naive();
        let v1 = out.iter().find(|(d, _)| *d == day1_local).map(|(_, v)| *v);
        let v2 = out.iter().find(|(d, _)| *d == day2_local).map(|(_, v)| *v);
        assert!(v1.is_some_and(|v| v > 0.0));
        assert!(v2.is_some_and(|v| v > 0.0));
    }

    #[test]
    fn deduplicates_lines_by_request_id_and_message_id() {
        // Real Claude Code sessions sometimes have the same assistant
        // line repeated (e.g. on resume). The (requestId|messageId)
        // key suppresses the second one. Two distinct lines with the
        // same message-id but different request-ids must still both
        // count.
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("p")).unwrap();
        let path = dir.path().join("p").join("s.jsonl");
        let mut f = std::fs::File::create(&path).unwrap();
        let ts = Utc::now().to_rfc3339();
        // Twice with the same (req, msg) → one count.
        write_assistant(&mut f, &ts, "req_1", "msg_1", "claude-opus-4-7", 100, 100);
        write_assistant(&mut f, &ts, "req_1", "msg_1", "claude-opus-4-7", 100, 100);
        // Different request → distinct count.
        write_assistant(&mut f, &ts, "req_2", "msg_1", "claude-opus-4-7", 100, 100);

        let cfg = cfg_for(dir.path());
        let out = anthropic_daily_spend_at(&cfg, 1, Utc::now() + Duration::seconds(1));
        let total: f64 = out.iter().map(|(_, v)| *v).sum();

        // Spend for one turn under default opus rates (input $15/M,
        // output $75/M for 100/100 tokens) ≈ 0.009. Two distinct
        // turns ≈ 0.018. We assert "roughly double the per-turn cost"
        // rather than an exact number so a future rate change doesn't
        // flake.
        let per_turn = (100.0 / 1_000_000.0) * 15.0 + (100.0 / 1_000_000.0) * 75.0;
        assert!(
            (total - per_turn * 2.0).abs() < 1e-9,
            "expected 2 distinct turns counted, got total={} per_turn={}",
            total,
            per_turn
        );
    }

    #[test]
    fn skips_records_outside_window() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("p")).unwrap();
        let path = dir.path().join("p").join("s.jsonl");
        let mut f = std::fs::File::create(&path).unwrap();

        // Far-in-the-past record: 30 days before "now".
        let old = Utc::now() - Duration::days(30);
        write_assistant(
            &mut f,
            &old.to_rfc3339(),
            "req_old",
            "msg_old",
            "claude-opus-4-7",
            1000,
            1000,
        );

        let cfg = cfg_for(dir.path());
        let out = anthropic_daily_spend_at(&cfg, 7, Utc::now());
        // Pre-seeded buckets all zero; out-of-window record is dropped.
        assert!(out.iter().all(|(_, v)| *v == 0.0));
    }

    #[test]
    fn ignores_non_assistant_records() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("p")).unwrap();
        let path = dir.path().join("p").join("s.jsonl");
        let mut f = std::fs::File::create(&path).unwrap();
        let ts = Utc::now().to_rfc3339();

        // A user-typed message — same shape, different `type`.
        let user_line = format!(
            r#"{{"type":"user","timestamp":"{}","requestId":"req_u","message":{{"model":"claude-opus-4-7","id":"msg_u","usage":{{"input_tokens":100,"output_tokens":100}}}}}}"#,
            ts
        );
        writeln!(f, "{}", user_line).unwrap();

        let cfg = cfg_for(dir.path());
        let out = anthropic_daily_spend_at(&cfg, 7, Utc::now());
        assert!(out.iter().all(|(_, v)| *v == 0.0));
    }

    #[test]
    fn ignores_malformed_lines() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("p")).unwrap();
        let path = dir.path().join("p").join("s.jsonl");
        let mut f = std::fs::File::create(&path).unwrap();

        writeln!(f, "not json at all").unwrap();
        writeln!(f, "{{ truncated json").unwrap();
        writeln!(f).unwrap(); // empty line
                              // …followed by one valid record that must still be counted.
        let ts = Utc::now().to_rfc3339();
        write_assistant(
            &mut f,
            &ts,
            "req_good",
            "msg_good",
            "claude-opus-4-7",
            100,
            100,
        );

        let cfg = cfg_for(dir.path());
        let out = anthropic_daily_spend_at(&cfg, 1, Utc::now() + Duration::seconds(1));
        let total: f64 = out.iter().map(|(_, v)| *v).sum();
        assert!(total > 0.0, "expected valid record to count: {:#?}", out);
    }

    #[test]
    fn missing_usage_block_is_skipped() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("p")).unwrap();
        let path = dir.path().join("p").join("s.jsonl");
        let mut f = std::fs::File::create(&path).unwrap();

        let ts = Utc::now().to_rfc3339();
        let no_usage = format!(
            r#"{{"type":"assistant","timestamp":"{}","requestId":"r","message":{{"model":"claude-opus-4-7","id":"m"}}}}"#,
            ts
        );
        writeln!(f, "{}", no_usage).unwrap();

        let cfg = cfg_for(dir.path());
        let out = anthropic_daily_spend_at(&cfg, 1, Utc::now() + Duration::seconds(1));
        assert!(out.iter().all(|(_, v)| *v == 0.0));
    }

    #[test]
    fn aggregates_across_multiple_project_dirs() {
        let dir = TempDir::new().unwrap();
        let ts = Utc::now().to_rfc3339();
        for (idx, proj) in ["alpha", "beta", "gamma"].iter().enumerate() {
            let p = dir.path().join(proj);
            std::fs::create_dir_all(&p).unwrap();
            let path = p.join(format!("session-{}.jsonl", idx));
            let mut f = std::fs::File::create(&path).unwrap();
            write_assistant(
                &mut f,
                &ts,
                &format!("req_{}", idx),
                &format!("msg_{}", idx),
                "claude-opus-4-7",
                100,
                100,
            );
        }

        let cfg = cfg_for(dir.path());
        let out = anthropic_daily_spend_at(&cfg, 1, Utc::now() + Duration::seconds(1));
        let total: f64 = out.iter().map(|(_, v)| *v).sum();
        // Three distinct turns at 100/100 each.
        let per_turn = (100.0 / 1_000_000.0) * 15.0 + (100.0 / 1_000_000.0) * 75.0;
        assert!(
            (total - per_turn * 3.0).abs() < 1e-9,
            "expected 3 turns counted; got total={} per_turn={}",
            total,
            per_turn
        );
    }

    #[test]
    fn invalid_timestamp_is_skipped() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("p")).unwrap();
        let path = dir.path().join("p").join("s.jsonl");
        let mut f = std::fs::File::create(&path).unwrap();
        // Valid JSON shape but timestamp won't parse as RFC3339.
        let bad_ts = r#"{"type":"assistant","timestamp":"not-a-date","requestId":"r","message":{"model":"claude-opus-4-7","id":"m","usage":{"input_tokens":10,"output_tokens":10}}}"#;
        writeln!(f, "{}", bad_ts).unwrap();

        let cfg = cfg_for(dir.path());
        let out = anthropic_daily_spend_at(&cfg, 1, Utc::now() + Duration::seconds(1));
        assert!(out.iter().all(|(_, v)| *v == 0.0));
    }

    #[test]
    fn output_is_sorted_ascending_by_date() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("p")).unwrap();
        let path = dir.path().join("p").join("s.jsonl");
        let mut f = std::fs::File::create(&path).unwrap();
        // Write records in reverse-chronological order to be sure the
        // sort isn't accidentally insertion-order.
        let t0 = Utc::now() - Duration::days(5);
        let t1 = Utc::now() - Duration::days(2);
        write_assistant(
            &mut f,
            &t1.to_rfc3339(),
            "r1",
            "m1",
            "claude-opus-4-7",
            50,
            50,
        );
        write_assistant(
            &mut f,
            &t0.to_rfc3339(),
            "r0",
            "m0",
            "claude-opus-4-7",
            50,
            50,
        );

        let cfg = cfg_for(dir.path());
        let out = anthropic_daily_spend_at(&cfg, 7, Utc::now());
        for w in out.windows(2) {
            assert!(w[0].0 <= w[1].0, "output not sorted: {:?}", out);
        }
    }

    #[test]
    fn model_rate_override_is_respected() {
        // A user can pin a custom rate for a given model substring via
        // `[anthropic.model_rates]` in config. Confirm the override
        // applies even when the rate is set absurdly high — the spend
        // should scale linearly.
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("p")).unwrap();
        let path = dir.path().join("p").join("s.jsonl");
        let mut f = std::fs::File::create(&path).unwrap();
        let ts = Utc::now().to_rfc3339();
        write_assistant(
            &mut f,
            &ts,
            "r",
            "m",
            "some-custom-model",
            1_000_000,
            1_000_000,
        );

        let mut cfg = cfg_for(dir.path());
        cfg.model_rates.insert(
            "some-custom-model".into(),
            llm_usage_core::pricing::ModelRate {
                input_per_mtok: 10.0,
                output_per_mtok: 20.0,
                cache_write_5m_mult: 0.0,
                cache_write_1h_mult: 0.0,
                cache_read_mult: 0.0,
            },
        );

        let out = anthropic_daily_spend_at(&cfg, 1, Utc::now() + Duration::seconds(1));
        let total: f64 = out.iter().map(|(_, v)| *v).sum();
        // 1M input × $10 + 1M output × $20 == $30.
        assert!((total - 30.0).abs() < 1e-9, "got {}", total);
    }
}
