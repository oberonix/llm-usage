//! Shared reader for opencode's per-message store
//! (`~/.local/share/opencode/opencode.db`).
//!
//! opencode (<https://opencode.ai>) persists every assistant turn it
//! makes, regardless of which upstream provider was hit, into a single
//! SQLite database. Each row carries token counts, the upstream
//! `providerID` (`openai`, `anthropic`, `ollama-cloud`, …), and the
//! completion timestamp. Users who hit OpenAI / Anthropic / Ollama
//! Cloud via opencode rather than the vendor's first-party tool
//! (codex CLI, Claude Code, raw `ollama` binary) have all their
//! activity here.
//!
//! Schema (verified against opencode 0.x, 2026-05):
//!
//! ```text
//! CREATE TABLE message (
//!   id           TEXT PRIMARY KEY,
//!   session_id   TEXT NOT NULL,
//!   time_created INTEGER NOT NULL,   -- unix millis
//!   time_updated INTEGER NOT NULL,
//!   data         TEXT NOT NULL       -- JSON blob, see below
//! );
//! ```
//!
//! Relevant fields inside `data`:
//!
//! ```json
//! {
//!   "role": "assistant",
//!   "providerID": "openai",
//!   "modelID": "gpt-5.5",
//!   "tokens": {
//!     "input": 1234, "output": 56, "reasoning": 0,
//!     "cache": { "read": 0, "write": 0 },
//!     "total": 1290
//!   },
//!   "cost": 0.0123,
//!   "time": { "created": 1778547061956, "completed": 1778547103548 }
//! }
//! ```
//!
//! We open the database read-only so opencode's WAL-mode writers are
//! never blocked, scan only the last 14 days (the longest window any
//! provider needs is 7 days), and skip aborted streams (rows with
//! zero tokens) so they don't show as request_count.

use anyhow::Result;
use chrono::{DateTime, Duration, TimeZone, Utc};
use rusqlite::{Connection, OpenFlags};
use serde_json::Value;
use std::path::{Path, PathBuf};

/// A single assistant turn as recorded by opencode.
#[derive(Debug, Clone)]
pub struct OpencodeEvent {
    /// `time.completed` (preferred) or `time_created` (fallback).
    pub timestamp: DateTime<Utc>,
    /// Upstream model id, e.g. `"gpt-5.5"`, `"claude-sonnet-4-5"`,
    /// `"deepseek-v4-pro"`. Empty when opencode didn't record one.
    pub model: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// Cache-read tokens (opencode's `data.tokens.cache.read`).
    pub cached_tokens: u64,
    /// Dollar cost opencode itself computed, or `None` if it didn't.
    pub cost_usd: Option<f64>,
}

/// Default path to the opencode store on Linux / macOS. Falls back to
/// a project-relative path when the OS data dir can't be resolved
/// (mostly relevant for tests).
pub fn default_db_path() -> PathBuf {
    dirs::data_dir()
        .map(|d| d.join("opencode").join("opencode.db"))
        .unwrap_or_else(|| PathBuf::from(".opencode.db"))
}

/// Read every assistant row in the last 14 days whose
/// `data.providerID` matches `provider_id`. Returns an empty Vec if
/// the database doesn't exist; bubbles up SQLite errors only.
pub fn read_events(db_path: &Path, provider_id: &str) -> Result<Vec<OpencodeEvent>> {
    if !db_path.exists() {
        return Ok(Vec::new());
    }
    let conn = Connection::open_with_flags(
        db_path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    let _ = conn.pragma_update(None, "query_only", true);

    let cutoff_ms = (Utc::now() - Duration::days(14)).timestamp_millis();
    let mut stmt = conn.prepare(
        "SELECT time_created, data FROM message WHERE time_created >= ?",
    )?;
    let mut rows = stmt.query([cutoff_ms])?;

    let mut events = Vec::new();
    while let Some(row) = rows.next()? {
        let created_ms: i64 = row.get(0)?;
        let blob: String = row.get(1)?;
        let v: Value = match serde_json::from_str(&blob) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if v.get("role").and_then(|x| x.as_str()) != Some("assistant") {
            continue;
        }
        // providerID can live at root or one level deep on a `model`
        // sub-object — accept either shape.
        let model_node = v
            .get("model")
            .cloned()
            .or_else(|| v.pointer("/parts/0/model").cloned());
        let prov = v
            .get("providerID")
            .and_then(|x| x.as_str())
            .or_else(|| {
                model_node
                    .as_ref()
                    .and_then(|m| m.get("providerID"))
                    .and_then(|x| x.as_str())
            })
            .unwrap_or("");
        if prov != provider_id {
            continue;
        }
        let model_id = v
            .get("modelID")
            .and_then(|x| x.as_str())
            .or_else(|| {
                model_node
                    .as_ref()
                    .and_then(|m| m.get("modelID"))
                    .and_then(|x| x.as_str())
            })
            .unwrap_or("")
            .to_string();
        let tokens = v.get("tokens");
        let input = tokens
            .and_then(|t| t.get("input"))
            .and_then(|x| x.as_u64())
            .unwrap_or(0);
        let output = tokens
            .and_then(|t| t.get("output"))
            .and_then(|x| x.as_u64())
            .unwrap_or(0);
        let cached = tokens
            .and_then(|t| t.get("cache"))
            .and_then(|c| c.get("read"))
            .and_then(|x| x.as_u64())
            .unwrap_or(0);
        if input == 0 && output == 0 && cached == 0 {
            continue;
        }
        let cost = v.get("cost").and_then(|x| x.as_f64()).filter(|c| *c > 0.0);
        let completed_ms = v
            .pointer("/time/completed")
            .and_then(|x| x.as_i64())
            .unwrap_or(created_ms);
        let Some(ts) = Utc.timestamp_millis_opt(completed_ms).single() else {
            continue;
        };
        events.push(OpencodeEvent {
            timestamp: ts,
            model: model_id,
            input_tokens: input,
            output_tokens: output,
            cached_tokens: cached,
            cost_usd: cost,
        });
    }
    Ok(events)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn write_fixture(
        path: &Path,
        provider_id: &str,
        completed_ms: i64,
        input: u64,
        output: u64,
        cost: Option<f64>,
    ) {
        let conn = Connection::open(path).unwrap();
        conn.execute_batch(
            r#"
            CREATE TABLE message (
                id TEXT PRIMARY KEY,
                session_id TEXT NOT NULL,
                time_created INTEGER NOT NULL,
                time_updated INTEGER NOT NULL,
                data TEXT NOT NULL
            );
            "#,
        )
        .unwrap();
        let mut data = serde_json::json!({
            "role": "assistant",
            "providerID": provider_id,
            "modelID": "model-x",
            "time": {"created": completed_ms - 500, "completed": completed_ms},
            "tokens": {
                "input": input,
                "output": output,
                "cache": {"read": 0, "write": 0},
                "reasoning": 0,
                "total": input + output,
            },
        });
        if let Some(c) = cost {
            data["cost"] = serde_json::json!(c);
        }
        conn.execute(
            "INSERT INTO message VALUES (?,?,?,?,?)",
            rusqlite::params!["a1", "ses1", completed_ms - 500, completed_ms, data.to_string()],
        )
        .unwrap();
    }

    #[test]
    fn missing_db_returns_empty() {
        let dir = TempDir::new().unwrap();
        let result = read_events(&dir.path().join("nope.db"), "openai").unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn extracts_cost_when_set() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("oc.db");
        let now_ms = chrono::Utc::now().timestamp_millis();
        write_fixture(&p, "anthropic", now_ms, 1_000, 200, Some(0.045));
        let events = read_events(&p, "anthropic").unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].cost_usd, Some(0.045));
    }

    #[test]
    fn skips_provider_mismatch() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("oc.db");
        let now_ms = chrono::Utc::now().timestamp_millis();
        write_fixture(&p, "ollama-cloud", now_ms, 500, 50, None);
        assert!(read_events(&p, "openai").unwrap().is_empty());
        assert_eq!(read_events(&p, "ollama-cloud").unwrap().len(), 1);
    }

    #[test]
    fn skips_zero_token_rows() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("oc.db");
        write_fixture(&p, "openai", chrono::Utc::now().timestamp_millis(), 0, 0, None);
        assert!(read_events(&p, "openai").unwrap().is_empty());
    }

    // Helper that creates the table once and lets the caller insert any
    // number of rows with arbitrary `data` JSON. Power-user version of
    // `write_fixture` for the schema/corner-case tests below.
    fn make_db(path: &Path) -> Connection {
        let conn = Connection::open(path).unwrap();
        conn.execute_batch(
            r#"
            CREATE TABLE message (
                id TEXT PRIMARY KEY,
                session_id TEXT NOT NULL,
                time_created INTEGER NOT NULL,
                time_updated INTEGER NOT NULL,
                data TEXT NOT NULL
            );
            "#,
        )
        .unwrap();
        conn
    }

    fn insert_row(conn: &Connection, id: &str, time_created_ms: i64, data: &str) {
        conn.execute(
            "INSERT INTO message VALUES (?,?,?,?,?)",
            rusqlite::params![id, "ses", time_created_ms, time_created_ms, data],
        )
        .unwrap();
    }

    #[test]
    fn skips_rows_with_corrupt_json_blob() {
        // A `data` column that doesn't parse must be skipped silently
        // — opencode's writer could emit half-truncated JSON during a
        // crash and we don't want to take the whole poll down.
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("oc.db");
        let now_ms = chrono::Utc::now().timestamp_millis();
        {
            let conn = make_db(&p);
            insert_row(&conn, "bad", now_ms, "{not valid json");
            insert_row(
                &conn,
                "good",
                now_ms,
                r#"{"role":"assistant","providerID":"openai","modelID":"gpt-x","time":{"completed":1778600000000},"tokens":{"input":10,"output":5,"cache":{"read":0}}}"#,
            );
        }
        let events = read_events(&p, "openai").unwrap();
        assert_eq!(events.len(), 1, "got: {:#?}", events);
        assert_eq!(events[0].input_tokens, 10);
    }

    #[test]
    fn excludes_rows_older_than_14_days() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("oc.db");
        let now_ms = chrono::Utc::now().timestamp_millis();
        let ancient_ms = now_ms - 1000 * 60 * 60 * 24 * 30; // 30 days ago
        let recent_ms = now_ms - 1000 * 60 * 60; // 1 hour ago
        {
            let conn = make_db(&p);
            for (id, t) in [("ancient", ancient_ms), ("recent", recent_ms)] {
                insert_row(
                    &conn,
                    id,
                    t,
                    &format!(
                        r#"{{"role":"assistant","providerID":"openai","modelID":"x","time":{{"completed":{t}}},"tokens":{{"input":100,"output":1}}}}"#,
                        t = t
                    ),
                );
            }
        }
        let events = read_events(&p, "openai").unwrap();
        assert_eq!(events.len(), 1, "old row must be filtered out: {:#?}", events);
    }

    #[test]
    fn falls_back_to_time_created_when_completed_missing() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("oc.db");
        let created_ms = chrono::Utc::now().timestamp_millis() - 60_000;
        {
            let conn = make_db(&p);
            // No `time.completed` — must use time_created.
            insert_row(
                &conn,
                "x",
                created_ms,
                r#"{"role":"assistant","providerID":"openai","modelID":"x","tokens":{"input":10,"output":1}}"#,
            );
        }
        let events = read_events(&p, "openai").unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].timestamp.timestamp_millis(), created_ms);
    }

    #[test]
    fn skips_non_assistant_roles() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("oc.db");
        let now_ms = chrono::Utc::now().timestamp_millis();
        {
            let conn = make_db(&p);
            insert_row(
                &conn,
                "u",
                now_ms,
                r#"{"role":"user","providerID":"openai","modelID":"x","tokens":{"input":100,"output":0}}"#,
            );
            insert_row(
                &conn,
                "a",
                now_ms,
                r#"{"role":"assistant","providerID":"openai","modelID":"x","tokens":{"input":5,"output":3}}"#,
            );
        }
        let events = read_events(&p, "openai").unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].input_tokens, 5);
    }

    #[test]
    fn cached_tokens_count_toward_non_zero_check() {
        // A row with input=output=0 but a non-zero cache.read is still
        // a real turn — should NOT be filtered.
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("oc.db");
        let now_ms = chrono::Utc::now().timestamp_millis();
        {
            let conn = make_db(&p);
            insert_row(
                &conn,
                "c",
                now_ms,
                r#"{"role":"assistant","providerID":"openai","modelID":"x","tokens":{"input":0,"output":0,"cache":{"read":500}}}"#,
            );
        }
        let events = read_events(&p, "openai").unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].cached_tokens, 500);
    }

    #[test]
    fn nested_provider_id_under_model_object_works() {
        // Newer opencode versions sometimes stash providerID inside
        // a `model` sub-object instead of at the root. The reader
        // accepts either shape.
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("oc.db");
        let now_ms = chrono::Utc::now().timestamp_millis();
        {
            let conn = make_db(&p);
            insert_row(
                &conn,
                "n",
                now_ms,
                r#"{"role":"assistant","model":{"providerID":"anthropic","modelID":"claude-x"},"tokens":{"input":1,"output":1}}"#,
            );
        }
        let events = read_events(&p, "anthropic").unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].model, "claude-x");
    }

    #[test]
    fn non_sqlite_file_returns_err() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("not-a-db.db");
        std::fs::write(&p, b"plain text, not sqlite").unwrap();
        assert!(read_events(&p, "openai").is_err());
    }

    #[test]
    fn schema_missing_message_table_returns_err() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("oc.db");
        let conn = Connection::open(&p).unwrap();
        // Valid SQLite file, but no `message` table — open succeeds,
        // prepare fails. Confirm the error surfaces rather than
        // silently returning an empty event list.
        conn.execute_batch("CREATE TABLE not_message (x INTEGER);").unwrap();
        drop(conn);
        assert!(read_events(&p, "openai").is_err());
    }

    #[test]
    fn aggregates_multiple_rows() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("oc.db");
        let now_ms = chrono::Utc::now().timestamp_millis();
        {
            let conn = make_db(&p);
            for (i, tokens) in [(1, 10), (2, 20), (3, 30)].iter().enumerate() {
                let blob = format!(
                    r#"{{"role":"assistant","providerID":"openai","modelID":"m","time":{{"completed":{}}},"tokens":{{"input":{},"output":1}}}}"#,
                    now_ms - i as i64 * 60_000,
                    tokens.1,
                );
                insert_row(&conn, &format!("r{}", tokens.0), now_ms - i as i64 * 60_000, &blob);
            }
        }
        let events = read_events(&p, "openai").unwrap();
        assert_eq!(events.len(), 3);
        let total_input: u64 = events.iter().map(|e| e.input_tokens).sum();
        assert_eq!(total_input, 60);
    }

    #[test]
    fn negative_cost_field_is_treated_as_unset() {
        // Defensive: opencode shouldn't emit negative costs, but a
        // signed-int wrap or bad import script could. The reader
        // filters `cost > 0`, so a negative or zero cost reads as
        // `None` rather than misrepresenting spend.
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("oc.db");
        let now_ms = chrono::Utc::now().timestamp_millis();
        {
            let conn = make_db(&p);
            insert_row(
                &conn,
                "neg",
                now_ms,
                r#"{"role":"assistant","providerID":"openai","modelID":"m","tokens":{"input":1,"output":1},"cost":-0.5}"#,
            );
            insert_row(
                &conn,
                "zero",
                now_ms,
                r#"{"role":"assistant","providerID":"openai","modelID":"m","tokens":{"input":1,"output":1},"cost":0.0}"#,
            );
        }
        let events = read_events(&p, "openai").unwrap();
        assert_eq!(events.len(), 2);
        assert!(events.iter().all(|e| e.cost_usd.is_none()));
    }
}
