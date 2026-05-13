use crate::model::{ProviderId, UsageSnapshot, WindowKind};
use crate::storage::Store;
use chrono::{DateTime, Datelike, Utc};
use std::sync::Arc;

#[derive(Debug, Clone)]
pub struct AlertEvent {
    pub provider: ProviderId,
    pub window_kind: String,
    pub window_id: String,
    pub threshold_pct: u32,
    pub fraction_used: f64,
    pub limit_usd: Option<f64>,
    pub spend_usd: Option<f64>,
    pub message: String,
}

pub struct QuotaEngine {
    store: Arc<Store>,
    /// Thresholds per provider (e.g. [0.5, 0.75, 0.9]).
    pub thresholds: Vec<(ProviderId, Vec<f64>)>,
}

impl QuotaEngine {
    pub fn new(store: Arc<Store>) -> Self {
        Self {
            store,
            thresholds: Vec::new(),
        }
    }

    pub fn set_thresholds(&mut self, provider: ProviderId, thresholds: Vec<f64>) {
        self.thresholds.retain(|(p, _)| *p != provider);
        self.thresholds.push((provider, thresholds));
    }

    pub fn evaluate(&self, snapshot: &UsageSnapshot) -> Vec<AlertEvent> {
        let Some(thresholds) = self
            .thresholds
            .iter()
            .find(|(p, _)| *p == snapshot.provider)
            .map(|(_, t)| t.clone())
        else {
            return Vec::new();
        };

        let mut alerts = Vec::new();
        for (label, window) in &snapshot.windows {
            let Some(frac) = window.fraction_used else {
                continue;
            };
            let window_id = window_id_for(label, snapshot.timestamp);
            for &t in &thresholds {
                if frac < t {
                    continue;
                }
                let pct = (t * 100.0).round() as u32;
                let already = self
                    .store
                    .alert_already_fired(&snapshot.provider.to_string(), label, &window_id, pct)
                    .ok()
                    .flatten()
                    .is_some();
                if already {
                    continue;
                }
                let _ = self.store.record_alert_fired(
                    &snapshot.provider.to_string(),
                    label,
                    &window_id,
                    pct,
                );
                let msg = format!(
                    "{} usage at {:.0}% of {} budget{}",
                    snapshot.provider.human(),
                    frac * 100.0,
                    label,
                    window
                        .limit_usd
                        .map(|l| format!(" (${:.2})", l))
                        .unwrap_or_default(),
                );
                alerts.push(AlertEvent {
                    provider: snapshot.provider,
                    window_kind: label.clone(),
                    window_id: window_id.clone(),
                    threshold_pct: pct,
                    fraction_used: frac,
                    limit_usd: window.limit_usd,
                    spend_usd: window.spend_usd,
                    message: msg,
                });
            }
        }
        alerts
    }
}

/// Stable window identifier so alerts only fire once per logical window.
/// e.g. "2026-W19" for week, "2026-05-08" for day, "2026-05" for month.
pub fn window_id_for(label: &str, t: DateTime<Utc>) -> String {
    match label {
        "today" => t.format("%Y-%m-%d").to_string(),
        "week" => {
            let iso = t.iso_week();
            format!("{}-W{:02}", iso.year(), iso.week())
        }
        "month" => t.format("%Y-%m").to_string(),
        "1h" => t.format("%Y-%m-%dT%H").to_string(),
        // 5h-rolling window has no clean ID; use the start time the provider supplied,
        // bucketed to the hour, so it changes each hour and we re-fire then if still over.
        "5h" => t.format("%Y-%m-%dT%H").to_string(),
        other => other.to_string(),
    }
}

/// Convenience helper for matching WindowKind to its label string.
pub fn label_of(kind: WindowKind) -> &'static str {
    kind.label()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ProviderStatus, WindowUsage};
    use std::collections::BTreeMap;

    fn snap_with_fraction(provider: ProviderId, label: &str, frac: f64) -> UsageSnapshot {
        let mut windows: BTreeMap<String, WindowUsage> = BTreeMap::new();
        windows.insert(
            label.to_string(),
            WindowUsage {
                fraction_used: Some(frac),
                ..Default::default()
            },
        );
        UsageSnapshot {
            provider,
            timestamp: Utc::now(),
            status: ProviderStatus::Ok,
            error: None,
            windows,
            headline: None,
            plan_label: None,
        }
    }

    #[test]
    fn fires_once_per_window_threshold() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let mut engine = QuotaEngine::new(store.clone());
        engine.set_thresholds(ProviderId::Anthropic, vec![0.5, 0.75, 0.9]);

        let mut windows: BTreeMap<String, WindowUsage> = BTreeMap::new();
        windows.insert(
            "week".into(),
            WindowUsage {
                spend_usd: Some(40.0),
                limit_usd: Some(50.0),
                fraction_used: Some(0.8),
                ..Default::default()
            },
        );
        let snap = UsageSnapshot {
            provider: ProviderId::Anthropic,
            timestamp: Utc::now(),
            status: ProviderStatus::Ok,
            error: None,
            windows,
            headline: None,
            plan_label: None,
        };

        let first = engine.evaluate(&snap);
        // 0.5 and 0.75 thresholds crossed (0.9 not yet).
        assert_eq!(first.len(), 2);

        let second = engine.evaluate(&snap);
        assert!(second.is_empty(), "should debounce");
    }

    #[test]
    fn evaluate_returns_empty_when_provider_has_no_thresholds() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let engine = QuotaEngine::new(store);
        let snap = snap_with_fraction(ProviderId::CodexCli, "week", 0.99);
        assert!(engine.evaluate(&snap).is_empty());
    }

    #[test]
    fn set_thresholds_replaces_previous_entry() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let mut engine = QuotaEngine::new(store);
        engine.set_thresholds(ProviderId::Anthropic, vec![0.5]);
        engine.set_thresholds(ProviderId::Anthropic, vec![0.9]);
        // Only one entry per provider, the latest one.
        let entries: Vec<_> = engine
            .thresholds
            .iter()
            .filter(|(p, _)| *p == ProviderId::Anthropic)
            .collect();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].1, vec![0.9]);
    }

    #[test]
    fn windows_without_fraction_are_ignored() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let mut engine = QuotaEngine::new(store);
        engine.set_thresholds(ProviderId::CodexCli, vec![0.5]);
        // Window present but no fraction → no alert.
        let mut windows: BTreeMap<String, WindowUsage> = BTreeMap::new();
        windows.insert("week".into(), WindowUsage::default());
        let snap = UsageSnapshot {
            provider: ProviderId::CodexCli,
            timestamp: Utc::now(),
            status: ProviderStatus::Ok,
            error: None,
            windows,
            headline: None,
            plan_label: None,
        };
        assert!(engine.evaluate(&snap).is_empty());
    }

    #[test]
    fn message_includes_provider_and_percentage() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let mut engine = QuotaEngine::new(store);
        engine.set_thresholds(ProviderId::OllamaCloud, vec![0.5]);
        let snap = snap_with_fraction(ProviderId::OllamaCloud, "week", 0.75);
        let alerts = engine.evaluate(&snap);
        assert_eq!(alerts.len(), 1);
        assert!(alerts[0].message.contains("Ollama Cloud"));
        assert!(alerts[0].message.contains("75%"));
        assert_eq!(alerts[0].threshold_pct, 50);
    }

    #[test]
    fn alerts_for_distinct_window_ids_each_fire() {
        // Same provider+threshold+window kind, but the window_id moves
        // forward (different week, day, etc.) — each should alert once.
        let store = Arc::new(Store::open_in_memory().unwrap());
        let mut engine = QuotaEngine::new(store);
        engine.set_thresholds(ProviderId::Anthropic, vec![0.5]);

        let make = |t: DateTime<Utc>| {
            let mut windows: BTreeMap<String, WindowUsage> = BTreeMap::new();
            windows.insert(
                "today".into(),
                WindowUsage {
                    fraction_used: Some(0.6),
                    ..Default::default()
                },
            );
            UsageSnapshot {
                provider: ProviderId::Anthropic,
                timestamp: t,
                status: ProviderStatus::Ok,
                error: None,
                windows,
                headline: None,
                plan_label: None,
            }
        };

        let day1 = Utc::now() - chrono::Duration::days(2);
        let day2 = Utc::now();
        assert_eq!(engine.evaluate(&make(day1)).len(), 1);
        assert_eq!(engine.evaluate(&make(day2)).len(), 1);
    }

    #[test]
    fn window_id_format_per_label() {
        let t = chrono::DateTime::parse_from_rfc3339("2026-05-08T13:45:00Z")
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(window_id_for("today", t), "2026-05-08");
        assert_eq!(window_id_for("month", t), "2026-05");
        // 1h and 5h share the hour-bucketed format.
        assert_eq!(window_id_for("1h", t), "2026-05-08T13");
        assert_eq!(window_id_for("5h", t), "2026-05-08T13");
        // Week is ISO-week ordinal.
        let week_id = window_id_for("week", t);
        assert!(week_id.starts_with("2026-W"), "got {}", week_id);
        // Unknown label is passed through.
        assert_eq!(window_id_for("custom", t), "custom");
    }

    #[test]
    fn label_of_matches_window_kind_label() {
        assert_eq!(label_of(WindowKind::Today), "today");
        assert_eq!(label_of(WindowKind::FiveHourRolling), "5h");
    }
}
