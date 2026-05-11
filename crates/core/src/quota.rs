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
                    .alert_already_fired(
                        &snapshot.provider.to_string(),
                        label,
                        &window_id,
                        pct,
                    )
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
        };

        let first = engine.evaluate(&snap);
        // 0.5 and 0.75 thresholds crossed (0.9 not yet).
        assert_eq!(first.len(), 2);

        let second = engine.evaluate(&snap);
        assert!(second.is_empty(), "should debounce");
    }
}
