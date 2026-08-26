use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::Mutex;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelTurnRecord {
    pub turn_id: String,
    pub provider: String,
    pub model: String,
    pub timestamp: DateTime<Utc>,
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub tokens_cached: u64,
    pub tokens_cache_creation: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QuotaSnapshotRecord {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<i64>,
    pub provider: String,
    pub timestamp: DateTime<Utc>,
    pub window_kind: String,
    pub fraction_used: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resets_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelStatsSummary {
    pub provider: String,
    pub model: String,
    pub turns_count: u64,
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub tokens_cached: u64,
    pub tokens_cache_creation: u64,
    pub total_tokens: u64,
}

pub struct Store {
    conn: Mutex<Connection>,
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let conn =
            Connection::open(path).with_context(|| format!("open sqlite {}", path.display()))?;
        let s = Self {
            conn: Mutex::new(conn),
        };
        s.init()?;
        Ok(s)
    }

    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        let s = Self {
            conn: Mutex::new(conn),
        };
        s.init()?;
        Ok(s)
    }

    fn init(&self) -> Result<()> {
        let conn = self.conn.lock().expect("poisoned");
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS alert_state (
                provider     TEXT NOT NULL,
                window_kind  TEXT NOT NULL,
                window_id    TEXT NOT NULL,
                threshold_pct INTEGER NOT NULL,
                fired_at_utc TEXT NOT NULL,
                PRIMARY KEY (provider, window_kind, window_id, threshold_pct)
            );

            CREATE TABLE IF NOT EXISTS provider_state (
                provider     TEXT PRIMARY KEY,
                last_poll_at TEXT NOT NULL,
                status       TEXT NOT NULL,
                error        TEXT
            );

            CREATE TABLE IF NOT EXISTS file_offsets (
                provider TEXT NOT NULL,
                path     TEXT NOT NULL,
                offset   INTEGER NOT NULL,
                inode    INTEGER,
                PRIMARY KEY (provider, path)
            );

            CREATE TABLE IF NOT EXISTS model_turns (
                turn_id               TEXT PRIMARY KEY,
                provider              TEXT NOT NULL,
                model                 TEXT NOT NULL,
                timestamp             TEXT NOT NULL,
                tokens_in             INTEGER NOT NULL,
                tokens_out            INTEGER NOT NULL,
                tokens_cached         INTEGER NOT NULL,
                tokens_cache_creation INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_model_turns_prov_time ON model_turns (provider, timestamp);
            CREATE INDEX IF NOT EXISTS idx_model_turns_model ON model_turns (model);

            CREATE TABLE IF NOT EXISTS quota_snapshots (
                id            INTEGER PRIMARY KEY AUTOINCREMENT,
                provider      TEXT NOT NULL,
                timestamp     TEXT NOT NULL,
                window_kind   TEXT NOT NULL,
                fraction_used REAL NOT NULL,
                resets_at     TEXT
            );
            CREATE INDEX IF NOT EXISTS idx_quota_snapshots_prov_time ON quota_snapshots (provider, timestamp);
            "#,
        )?;
        Ok(())
    }

    pub fn record_alert_fired(
        &self,
        provider: &str,
        window_kind: &str,
        window_id: &str,
        threshold_pct: u32,
    ) -> Result<()> {
        let conn = self.conn.lock().expect("poisoned");
        conn.execute(
            "INSERT OR REPLACE INTO alert_state \
             (provider, window_kind, window_id, threshold_pct, fired_at_utc) \
             VALUES (?, ?, ?, ?, ?)",
            params![
                provider,
                window_kind,
                window_id,
                threshold_pct,
                Utc::now().to_rfc3339(),
            ],
        )?;
        Ok(())
    }

    pub fn alert_already_fired(
        &self,
        provider: &str,
        window_kind: &str,
        window_id: &str,
        threshold_pct: u32,
    ) -> Result<Option<DateTime<Utc>>> {
        let conn = self.conn.lock().expect("poisoned");
        let row: Option<String> = conn
            .query_row(
                "SELECT fired_at_utc FROM alert_state \
                 WHERE provider = ? AND window_kind = ? AND window_id = ? AND threshold_pct = ?",
                params![provider, window_kind, window_id, threshold_pct],
                |r| r.get(0),
            )
            .optional()?;
        Ok(row.and_then(|s| {
            DateTime::parse_from_rfc3339(&s)
                .ok()
                .map(|d| d.with_timezone(&Utc))
        }))
    }

    pub fn record_provider_state(
        &self,
        provider: &str,
        status: &str,
        error: Option<&str>,
    ) -> Result<()> {
        let conn = self.conn.lock().expect("poisoned");
        conn.execute(
            "INSERT OR REPLACE INTO provider_state (provider, last_poll_at, status, error) \
             VALUES (?, ?, ?, ?)",
            params![provider, Utc::now().to_rfc3339(), status, error],
        )?;
        Ok(())
    }

    pub fn record_model_turns_batch(&self, turns: &[ModelTurnRecord]) -> Result<usize> {
        if turns.is_empty() {
            return Ok(0);
        }
        let mut conn = self.conn.lock().expect("poisoned");
        let tx = conn.transaction()?;
        let mut inserted = 0;
        {
            let mut stmt = tx.prepare(
                "INSERT OR IGNORE INTO model_turns \
                 (turn_id, provider, model, timestamp, tokens_in, tokens_out, tokens_cached, tokens_cache_creation) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
            )?;
            for turn in turns {
                let count = stmt.execute(params![
                    turn.turn_id,
                    turn.provider,
                    turn.model,
                    turn.timestamp.to_rfc3339(),
                    turn.tokens_in as i64,
                    turn.tokens_out as i64,
                    turn.tokens_cached as i64,
                    turn.tokens_cache_creation as i64,
                ])?;
                inserted += count;
            }
        }
        tx.commit()?;
        Ok(inserted)
    }

    pub fn record_quota_snapshot(&self, snap: &QuotaSnapshotRecord) -> Result<()> {
        let conn = self.conn.lock().expect("poisoned");
        conn.execute(
            "INSERT INTO quota_snapshots \
             (provider, timestamp, window_kind, fraction_used, resets_at) \
             VALUES (?, ?, ?, ?, ?)",
            params![
                snap.provider,
                snap.timestamp.to_rfc3339(),
                snap.window_kind,
                snap.fraction_used,
                snap.resets_at.map(|d| d.to_rfc3339()),
            ],
        )?;
        Ok(())
    }

    pub fn query_model_stats(
        &self,
        provider: Option<&str>,
        since: Option<DateTime<Utc>>,
        until: Option<DateTime<Utc>>,
    ) -> Result<Vec<ModelStatsSummary>> {
        let conn = self.conn.lock().expect("poisoned");
        let mut sql = String::from(
            "SELECT provider, model, COUNT(*), \
             COALESCE(SUM(tokens_in), 0), \
             COALESCE(SUM(tokens_out), 0), \
             COALESCE(SUM(tokens_cached), 0), \
             COALESCE(SUM(tokens_cache_creation), 0) \
             FROM model_turns WHERE 1=1",
        );
        let mut params_vec: Vec<rusqlite::types::Value> = Vec::new();
        if let Some(p) = provider {
            sql.push_str(" AND provider = ?");
            params_vec.push(p.to_string().into());
        }
        if let Some(s) = since {
            sql.push_str(" AND timestamp >= ?");
            params_vec.push(s.to_rfc3339().into());
        }
        if let Some(u) = until {
            sql.push_str(" AND timestamp <= ?");
            params_vec.push(u.to_rfc3339().into());
        }
        sql.push_str(" GROUP BY provider, model ORDER BY provider, model");

        let mut stmt = conn.prepare(&sql)?;
        let rusqlite_params = rusqlite::params_from_iter(params_vec);
        let rows = stmt.query_map(rusqlite_params, |r| {
            let provider: String = r.get(0)?;
            let model: String = r.get(1)?;
            let count: i64 = r.get(2)?;
            let in_tok: i64 = r.get(3)?;
            let out_tok: i64 = r.get(4)?;
            let cached_tok: i64 = r.get(5)?;
            let cache_creat_tok: i64 = r.get(6)?;
            let tokens_in = in_tok.max(0) as u64;
            let tokens_out = out_tok.max(0) as u64;
            let tokens_cached = cached_tok.max(0) as u64;
            let tokens_cache_creation = cache_creat_tok.max(0) as u64;
            let total_tokens = tokens_in
                .saturating_add(tokens_out)
                .saturating_add(tokens_cached)
                .saturating_add(tokens_cache_creation);
            Ok(ModelStatsSummary {
                provider,
                model,
                turns_count: count.max(0) as u64,
                tokens_in,
                tokens_out,
                tokens_cached,
                tokens_cache_creation,
                total_tokens,
            })
        })?;

        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    pub fn query_quota_snapshots(
        &self,
        provider: Option<&str>,
        since: Option<DateTime<Utc>>,
        until: Option<DateTime<Utc>>,
    ) -> Result<Vec<QuotaSnapshotRecord>> {
        let conn = self.conn.lock().expect("poisoned");
        let mut sql = String::from(
            "SELECT id, provider, timestamp, window_kind, fraction_used, resets_at \
             FROM quota_snapshots WHERE 1=1",
        );
        let mut params_vec: Vec<rusqlite::types::Value> = Vec::new();
        if let Some(p) = provider {
            sql.push_str(" AND provider = ?");
            params_vec.push(p.to_string().into());
        }
        if let Some(s) = since {
            sql.push_str(" AND timestamp >= ?");
            params_vec.push(s.to_rfc3339().into());
        }
        if let Some(u) = until {
            sql.push_str(" AND timestamp <= ?");
            params_vec.push(u.to_rfc3339().into());
        }
        sql.push_str(" ORDER BY timestamp ASC, id ASC");

        let mut stmt = conn.prepare(&sql)?;
        let rusqlite_params = rusqlite::params_from_iter(params_vec);
        let rows = stmt.query_map(rusqlite_params, |r| {
            let id: i64 = r.get(0)?;
            let provider: String = r.get(1)?;
            let ts_str: String = r.get(2)?;
            let window_kind: String = r.get(3)?;
            let fraction_used: f64 = r.get(4)?;
            let resets_at_str: Option<String> = r.get(5)?;

            let timestamp = DateTime::parse_from_rfc3339(&ts_str)
                .ok()
                .map(|d| d.with_timezone(&Utc))
                .unwrap_or_else(Utc::now);

            let resets_at = resets_at_str.and_then(|s| {
                DateTime::parse_from_rfc3339(&s)
                    .ok()
                    .map(|d| d.with_timezone(&Utc))
            });

            Ok(QuotaSnapshotRecord {
                id: Some(id),
                provider,
                timestamp,
                window_kind,
                fraction_used,
                resets_at,
            })
        })?;

        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alert_dedupe() {
        let s = Store::open_in_memory().unwrap();
        assert!(s
            .alert_already_fired("anthropic", "week", "2026-W19", 75)
            .unwrap()
            .is_none());
        s.record_alert_fired("anthropic", "week", "2026-W19", 75)
            .unwrap();
        assert!(s
            .alert_already_fired("anthropic", "week", "2026-W19", 75)
            .unwrap()
            .is_some());
    }

    #[test]
    fn alert_dedupe_keyed_on_all_four_columns() {
        // Different threshold → independent dedupe entry.
        let s = Store::open_in_memory().unwrap();
        s.record_alert_fired("anthropic", "week", "2026-W19", 50)
            .unwrap();
        assert!(s
            .alert_already_fired("anthropic", "week", "2026-W19", 50)
            .unwrap()
            .is_some());
        assert!(s
            .alert_already_fired("anthropic", "week", "2026-W19", 75)
            .unwrap()
            .is_none());
        // Different provider → independent dedupe entry.
        assert!(s
            .alert_already_fired("codex_cli", "week", "2026-W19", 50)
            .unwrap()
            .is_none());
        // Different window id → independent dedupe entry.
        assert!(s
            .alert_already_fired("anthropic", "week", "2026-W20", 50)
            .unwrap()
            .is_none());
        // Different window kind → independent dedupe entry.
        assert!(s
            .alert_already_fired("anthropic", "month", "2026-W19", 50)
            .unwrap()
            .is_none());
    }

    #[test]
    fn alert_fired_returns_recent_timestamp() {
        let s = Store::open_in_memory().unwrap();
        let before = Utc::now();
        s.record_alert_fired("anthropic", "today", "2026-05-10", 90)
            .unwrap();
        let at = s
            .alert_already_fired("anthropic", "today", "2026-05-10", 90)
            .unwrap()
            .unwrap();
        // Some clock skew is acceptable; just sanity-check we're in the right ballpark.
        assert!(at >= before - chrono::Duration::seconds(2));
        assert!(at <= Utc::now() + chrono::Duration::seconds(2));
    }

    #[test]
    fn record_alert_fired_is_idempotent_on_same_key() {
        // INSERT OR REPLACE behaviour: second call overwrites timestamp.
        let s = Store::open_in_memory().unwrap();
        s.record_alert_fired("anthropic", "week", "2026-W19", 75)
            .unwrap();
        // Should not error.
        s.record_alert_fired("anthropic", "week", "2026-W19", 75)
            .unwrap();
    }

    #[test]
    fn provider_state_is_upsertable() {
        let s = Store::open_in_memory().unwrap();
        s.record_provider_state("anthropic", "Ok", None).unwrap();
        s.record_provider_state("anthropic", "Degraded", Some("rate limited"))
            .unwrap();
        // Should not blow up; cannot read back without an accessor but
        // exercising the write path is what matters here.
    }

    #[test]
    fn model_turns_batch_insert_and_stats() {
        let s = Store::open_in_memory().unwrap();
        let now = Utc::now();
        let turns = vec![
            ModelTurnRecord {
                turn_id: "turn-1".into(),
                provider: "anthropic".into(),
                model: "claude-opus-4-7".into(),
                timestamp: now - chrono::Duration::minutes(10),
                tokens_in: 100,
                tokens_out: 50,
                tokens_cached: 20,
                tokens_cache_creation: 10,
            },
            ModelTurnRecord {
                turn_id: "turn-2".into(),
                provider: "anthropic".into(),
                model: "claude-opus-4-7".into(),
                timestamp: now - chrono::Duration::minutes(5),
                tokens_in: 200,
                tokens_out: 100,
                tokens_cached: 0,
                tokens_cache_creation: 0,
            },
            ModelTurnRecord {
                turn_id: "turn-3".into(),
                provider: "codex_cli".into(),
                model: "gpt-5-codex".into(),
                timestamp: now - chrono::Duration::minutes(2),
                tokens_in: 300,
                tokens_out: 40,
                tokens_cached: 50,
                tokens_cache_creation: 0,
            },
        ];

        let inserted = s.record_model_turns_batch(&turns).unwrap();
        assert_eq!(inserted, 3);

        // Re-inserting turn-1 should be ignored (dedupe)
        let dup = vec![turns[0].clone()];
        let inserted_dup = s.record_model_turns_batch(&dup).unwrap();
        assert_eq!(inserted_dup, 0);

        let stats = s.query_model_stats(None, None, None).unwrap();
        assert_eq!(stats.len(), 2);
        assert_eq!(stats[0].model, "claude-opus-4-7");
        assert_eq!(stats[0].turns_count, 2);
        assert_eq!(stats[0].tokens_in, 300);
        assert_eq!(stats[0].tokens_out, 150);
        assert_eq!(stats[0].tokens_cached, 20);
        assert_eq!(stats[0].tokens_cache_creation, 10);
        assert_eq!(stats[0].total_tokens, 480);

        assert_eq!(stats[1].model, "gpt-5-codex");
        assert_eq!(stats[1].turns_count, 1);
        assert_eq!(stats[1].total_tokens, 390);

        // Filter by provider
        let anthropic_stats = s.query_model_stats(Some("anthropic"), None, None).unwrap();
        assert_eq!(anthropic_stats.len(), 1);
        assert_eq!(anthropic_stats[0].provider, "anthropic");
    }

    #[test]
    fn quota_snapshot_record_and_query() {
        let s = Store::open_in_memory().unwrap();
        let now = Utc::now();
        let snap1 = QuotaSnapshotRecord {
            id: None,
            provider: "anthropic".into(),
            timestamp: now - chrono::Duration::minutes(15),
            window_kind: "5h".into(),
            fraction_used: 0.25,
            resets_at: Some(now + chrono::Duration::hours(3)),
        };
        let snap2 = QuotaSnapshotRecord {
            id: None,
            provider: "anthropic".into(),
            timestamp: now,
            window_kind: "5h".into(),
            fraction_used: 0.35,
            resets_at: Some(now + chrono::Duration::hours(3)),
        };
        s.record_quota_snapshot(&snap1).unwrap();
        s.record_quota_snapshot(&snap2).unwrap();

        let rows = s.query_quota_snapshots(Some("anthropic"), None, None).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].fraction_used, 0.25);
        assert_eq!(rows[1].fraction_used, 0.35);
    }
}
