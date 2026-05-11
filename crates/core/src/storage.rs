use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use rusqlite::{params, Connection, OptionalExtension};
use std::path::Path;
use std::sync::Mutex;

pub struct Store {
    conn: Mutex<Connection>,
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let conn = Connection::open(path)
            .with_context(|| format!("open sqlite {}", path.display()))?;
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
        Ok(row.and_then(|s| DateTime::parse_from_rfc3339(&s).ok().map(|d| d.with_timezone(&Utc))))
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
}
