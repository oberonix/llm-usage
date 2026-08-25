//! Serializer and exporter for Omarchy's agent usage panel (`omarchy.agents`).
//!
//! Omarchy discovers any JSON file in `~/.local/state/omarchy/agents/usage/*.json`
//! and renders a tab / meter for each agent. This module converts [`UsageSnapshot`]
//! instances into Omarchy's expected JSON record format and atomically writes them
//! to the Omarchy usage state directory.

use crate::model::{ProviderId, ProviderStatus, UsageSnapshot, WindowKind};
use anyhow::{Context, Result};
use chrono::Duration;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct OmarchyLimit {
    pub label: String,
    pub percent: f64,
    #[serde(rename = "resetsAt", default)]
    pub resets_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct OmarchyRecentDay {
    pub date: String,
    #[serde(rename = "messageCount")]
    pub message_count: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct OmarchyModelUsage {
    #[serde(rename = "inputTokens")]
    pub input_tokens: u64,
    #[serde(rename = "outputTokens")]
    pub output_tokens: u64,
    #[serde(rename = "cacheReadInputTokens")]
    pub cache_read_input_tokens: u64,
    #[serde(rename = "cacheCreationInputTokens")]
    pub cache_creation_input_tokens: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct OmarchyAgentRecord {
    #[serde(rename = "schemaVersion")]
    pub schema_version: u32,
    pub id: String,
    pub name: String,
    #[serde(rename = "updatedAt")]
    pub updated_at: String,
    pub ready: bool,
    #[serde(rename = "hasLocalStats")]
    pub has_local_stats: bool,
    #[serde(rename = "tierLabel")]
    pub tier_label: String,
    #[serde(rename = "usageStatusText")]
    pub usage_status_text: String,
    #[serde(rename = "authHelpText")]
    pub auth_help_text: String,
    pub limits: Vec<OmarchyLimit>,
    #[serde(rename = "todayPrompts")]
    pub today_prompts: u64,
    #[serde(rename = "todaySessions")]
    pub today_sessions: u64,
    #[serde(rename = "todayTotalTokens")]
    pub today_total_tokens: u64,
    #[serde(rename = "todayTokensByModel")]
    pub today_tokens_by_model: BTreeMap<String, u64>,
    #[serde(rename = "recentDays")]
    pub recent_days: Vec<OmarchyRecentDay>,
    #[serde(rename = "totalPrompts")]
    pub total_prompts: u64,
    #[serde(rename = "totalSessions")]
    pub total_sessions: u64,
    #[serde(rename = "activeDays")]
    pub active_days: u64,
    #[serde(rename = "activeDates")]
    pub active_dates: Vec<String>,
    #[serde(rename = "modelUsage")]
    pub model_usage: BTreeMap<String, OmarchyModelUsage>,
}

/// Map ProviderId to the filename slug and Omarchy display name.
pub fn provider_omarchy_meta(provider: ProviderId) -> (&'static str, &'static str) {
    match provider {
        ProviderId::Antigravity => ("antigravity", "Antigravity"),
        ProviderId::Anthropic => ("claude", "Claude Code"),
        ProviderId::CodexCli => ("codex", "Codex"),
        ProviderId::OllamaCloud => ("ollama", "Ollama Cloud"),
    }
}

/// Convert an internal [`UsageSnapshot`] into Omarchy's record schema.
pub fn snapshot_to_omarchy_record(snap: &UsageSnapshot) -> OmarchyAgentRecord {
    let (id_slug, display_name) = provider_omarchy_meta(snap.provider);
    let ready = matches!(snap.status, ProviderStatus::Ok | ProviderStatus::Degraded);
    let tier_label = snap.plan_label.clone().unwrap_or_default();
    let usage_status_text = match snap.status {
        ProviderStatus::Unavailable => snap
            .error
            .clone()
            .unwrap_or_else(|| "Unavailable".to_string()),
        ProviderStatus::Degraded => snap
            .error
            .clone()
            .unwrap_or_else(|| "Degraded".to_string()),
        ProviderStatus::Ok => String::new(),
    };

    let mut limits = Vec::new();
    for (label, w) in &snap.windows {
        if let Some(frac) = w.fraction_used {
            let omarchy_label = match label.as_str() {
                "5h" => "5h window".to_string(),
                "week" => "Weekly (7-day)".to_string(),
                "month" => "Monthly (30-day)".to_string(),
                other => other.to_string(),
            };
            let resets_at = w
                .ends_at
                .map(|dt| dt.to_rfc3339())
                .unwrap_or_default();
            limits.push(OmarchyLimit {
                label: omarchy_label,
                percent: frac.clamp(0.0, 2.0),
                resets_at,
            });
        }
    }

    let today_window = snap.window(WindowKind::Today);
    let today_prompts = today_window.map(|w| w.request_count).unwrap_or(0);
    let today_total_tokens = today_window
        .map(|w| w.tokens_in.saturating_add(w.tokens_out))
        .unwrap_or(0);

    let week_window = snap.window(WindowKind::ThisWeek);
    let total_prompts = week_window.map(|w| w.request_count).unwrap_or(today_prompts);

    let now = snap.timestamp;
    let today_str = now.format("%Y-%m-%d").to_string();

    // 7-day trailing histogram (day-6 .. today)
    let mut recent_days = Vec::with_capacity(7);
    for offset in (0..7).rev() {
        let day_date = (now - Duration::days(offset)).format("%Y-%m-%d").to_string();
        let count = if day_date == today_str {
            today_total_tokens
        } else {
            0
        };
        recent_days.push(OmarchyRecentDay {
            date: day_date,
            message_count: count,
        });
    }

    let mut active_dates = Vec::new();
    if today_total_tokens > 0 || today_prompts > 0 {
        active_dates.push(today_str);
    }
    let active_days = active_dates.len() as u64;

    OmarchyAgentRecord {
        schema_version: 1,
        id: id_slug.to_string(),
        name: display_name.to_string(),
        updated_at: snap.timestamp.to_rfc3339(),
        ready,
        has_local_stats: true,
        tier_label,
        usage_status_text,
        auth_help_text: String::new(),
        limits,
        today_prompts,
        today_sessions: if today_prompts > 0 { 1 } else { 0 },
        today_total_tokens,
        today_tokens_by_model: BTreeMap::new(),
        recent_days,
        total_prompts,
        total_sessions: if total_prompts > 0 { 1 } else { 0 },
        active_days,
        active_dates,
        model_usage: BTreeMap::new(),
    }
}

/// Target directory for Omarchy usage files: `~/.local/state/omarchy/agents/usage`.
pub fn omarchy_usage_dir() -> Option<PathBuf> {
    if let Ok(val) = std::env::var("OMARCHY_AGENT_USAGE_DIR") {
        return Some(PathBuf::from(val));
    }
    let state_home = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|h| h.join(".local/state")))?;
    Some(state_home.join("omarchy/agents/usage"))
}

/// Export all snapshots to Omarchy usage JSON files.
pub fn export_snapshots_to_omarchy(snapshots: &BTreeMap<ProviderId, UsageSnapshot>) -> Result<()> {
    let Some(dir) = omarchy_usage_dir() else {
        return Ok(());
    };
    std::fs::create_dir_all(&dir).with_context(|| format!("create omarchy usage dir {}", dir.display()))?;

    for snap in snapshots.values() {
        let (id_slug, _) = provider_omarchy_meta(snap.provider);
        let record = snapshot_to_omarchy_record(snap);
        let target_path = dir.join(format!("{}.json", id_slug));
        let json_bytes = serde_json::to_vec_pretty(&record)
            .with_context(|| format!("serialize omarchy record for {}", id_slug))?;
        write_atomically(&target_path, &json_bytes)?;
    }
    Ok(())
}

fn write_atomically(target: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = target.with_extension("tmp");
    std::fs::write(&tmp, bytes).with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, target)
        .with_context(|| format!("rename {} -> {}", tmp.display(), target.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ProviderId, ProviderStatus, UsageSnapshot, WindowKind, WindowUsage};
    use chrono::Utc;
    use tempfile::TempDir;

    #[test]
    fn snapshot_to_omarchy_record_maps_antigravity() {
        let now = Utc::now();
        let mut snap = UsageSnapshot {
            provider: ProviderId::Antigravity,
            timestamp: now,
            status: ProviderStatus::Ok,
            error: None,
            windows: BTreeMap::new(),
            headline: Some("Gemini 42%".into()),
            plan_label: Some("Pro".into()),
        };
        snap.windows.insert(
            "5h".into(),
            WindowUsage {
                fraction_used: Some(0.42),
                ends_at: Some(now + Duration::hours(2)),
                ..Default::default()
            },
        );
        let today = snap.window_mut(WindowKind::Today);
        today.tokens_in = 1000;
        today.tokens_out = 500;
        today.request_count = 3;

        let record = snapshot_to_omarchy_record(&snap);
        assert_eq!(record.id, "antigravity");
        assert_eq!(record.name, "Antigravity");
        assert_eq!(record.tier_label, "Pro");
        assert!(record.ready);
        assert_eq!(record.today_prompts, 3);
        assert_eq!(record.today_total_tokens, 1500);
        assert_eq!(record.limits.len(), 1);
        assert_eq!(record.limits[0].label, "5h window");
        assert_eq!(record.limits[0].percent, 0.42);
        assert_eq!(record.recent_days.len(), 7);
        assert_eq!(record.recent_days.last().unwrap().message_count, 1500);
    }

    #[test]
    fn export_snapshots_writes_files_atomically() {
        let temp_dir = TempDir::new().unwrap();
        std::env::set_var("OMARCHY_AGENT_USAGE_DIR", temp_dir.path());

        let mut snaps = BTreeMap::new();
        let snap = UsageSnapshot {
            provider: ProviderId::Antigravity,
            timestamp: Utc::now(),
            status: ProviderStatus::Ok,
            error: None,
            windows: BTreeMap::new(),
            headline: None,
            plan_label: Some("Pro".into()),
        };
        snaps.insert(ProviderId::Antigravity, snap);

        export_snapshots_to_omarchy(&snaps).unwrap();
        let out_file = temp_dir.path().join("antigravity.json");
        assert!(out_file.exists());
        let raw = std::fs::read_to_string(&out_file).unwrap();
        let parsed: OmarchyAgentRecord = serde_json::from_str(&raw).unwrap();
        assert_eq!(parsed.id, "antigravity");
        assert_eq!(parsed.name, "Antigravity");

        std::env::remove_var("OMARCHY_AGENT_USAGE_DIR");
    }
}
